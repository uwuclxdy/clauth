mod actions;
mod alibaba_login;
mod claude;
mod claude_json;
mod cli;
mod codex_profiles;
mod completions;
mod daemon;
mod fallback;
mod format;
mod harness;
mod herdr;
mod hook_note;
mod jobs_cli;
mod jsonsync;
// macOS-only: Claude Code reads its login from the Keychain, not the credentials
// file, so a switch must also write there. Gated so non-macOS builds stay clean.
#[cfg(target_os = "macos")]
mod keychain;
mod list;
mod live_sessions;
mod lock;
mod lockorder;
mod logline;
mod mcp;
mod oauth;
mod oauth_login;
mod out;
mod platform;
mod plugin_host;
mod plugin_probe;
mod poll;
mod presets;
mod pricing;
mod profile;
mod profile_cache;
mod profile_json;
mod providers;
mod runtime;
mod sessions;
mod sessions_cli;
mod settings_sync;
mod spinner;
mod start;
mod status;
mod throughput;
mod token_ledger;
mod tokens;
mod tui;
mod update;
mod usage;
mod watchdog;
mod which;

#[cfg(test)]
mod testutil;

use anyhow::{Context, Result};
use clap::Parser as _;

use crate::cli::{Cli, Command, LoginArgs, ThemeArg};
use crate::out::{errln, out, outln};
use crate::profile::{AppConfig, ProfileName, ThemeName, load_config};
use crate::runtime::Isolation;

/// Resolve `name` to its canonical spelling, or bail with a [`UsageError`].
/// A bare unrecognized word lands here as a profile name (clap's `external`
/// subcommand), so a typo'd subcommand and a typo'd profile name are
/// indistinguishable at this position. Either way the caller named something
/// that isn't there: a usage error (exit 2), not a runtime failure (exit 1).
/// Shared by every profile-naming command: `start`/`delete`/`disable`/
/// `enable`/`switch`/`rolling-token`/`static-token`.
fn resolve_or_bail(config: &AppConfig, name: &str) -> Result<ProfileName> {
    config
        .canonical_name(name)
        .map(ProfileName::from)
        .ok_or_else(|| {
            let available = config.names().join(", ");
            usage_error(format!(
                "profile '{name}' not found\navailable: {available}"
            ))
        })
}

fn main() {
    // `Error::exit` prints help/version to stdout and exits 0, and any real
    // parse error to stderr and exits 2 — which is already the usage-error half
    // of the exit contract [`exit_code`] owns for the rest.
    let cli = Cli::try_parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit());
    std::process::exit(exit_code(dispatch(cli)));
}

/// A usage error (bad flag/args) for the sessions-surface commands. Distinct
/// from a runtime failure so [`exit_code`] can map it to process exit 2, while a
/// genuine error (including "no sessions found") stays exit 1.
#[derive(Debug)]
pub(crate) struct UsageError(pub(crate) String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Build a [`UsageError`] as an `anyhow::Error` for a dispatch arm to return.
fn usage_error(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(msg.into()))
}

/// Map a dispatch outcome to a process exit code: 0 on success, 2 for a
/// [`UsageError`] (bad flag/args), 1 for any other failure. Prints the error
/// exactly as anyhow's `Result` `Termination` did (`Error: {:?}`), so the
/// message surface is unchanged now that `main` maps the code itself.
pub(crate) fn exit_code(result: Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            // `errln!`, so a reader that walked away from `2>&1 | head` still
            // gets this code rather than the 101 `eprintln!` panicked with.
            errln!("Error: {e:?}");
            if e.downcast_ref::<UsageError>().is_some() {
                2
            } else {
                1
            }
        }
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    // `--theme` is a root-level global, so it parses ahead of any subcommand
    // and is accepted (and ignored) on the non-TUI paths.
    let theme_override = cli.theme.map(|t| match t {
        ThemeArg::Full => tui::theme::Tier::Full,
        ThemeArg::Compatible => tui::theme::Tier::Compatible,
    });

    let Some(command) = cli.command else {
        return cmd_tui(theme_override);
    };

    match command {
        Command::Start(a) => cmd_start(&a.profile, &a.claude_args, a.isolation(), a.with_fallback),
        Command::Login(a) => cmd_login(a),
        Command::Delete {
            profile,
            yes,
            force,
        } => cmd_delete(&profile, yes, force),
        Command::Disable { profile, yes } => cmd_disable(&profile, yes),
        Command::StaticToken {
            profile,
            clear,
            yes,
        } => {
            if clear {
                cmd_static_token_clear(&profile, yes)
            } else {
                cmd_static_token(&profile)
            }
        }
        Command::Enable { profile } => cmd_enable(&profile),
        Command::RollingToken { profile } => cmd_rolling_token(&profile),
        Command::Which { json } => which::run(json),
        Command::List { all, disabled } => list::run(all || disabled),
        Command::Jobs { json } => jobs_cli::run(json),
        Command::Sessions { json, tokens } => sessions_cli::run_sessions(json, tokens),
        Command::Resume { target, profile } => {
            sessions_cli::run_resume(&target, profile.as_deref())
        }
        Command::Info { target } => sessions_cli::run_info(&target),
        Command::Daemon {
            standby,
            replace,
            status,
            // The default's explicit spelling: nothing to branch on.
            no_standby: _,
        } => cmd_daemon(standby, replace, status),
        Command::Status {
            json: _,
            all,
            disabled,
        } => daemon::status_oneshot(all || disabled),
        Command::Mcp => mcp::serve(),
        Command::McpAwaitJob => mcp::await_job(),
        Command::HookProfileChangedNote => hook_note::run(),
        // The dispatch itself is one line and deliberately unpinned: a test
        // that ran it would run the real lifecycle leg. The name is pinned by
        // the parse pin in `tests/inline/cli.rs`, and the leg it points at is
        // pinned hermetically by the fake-`claude` tests.
        Command::SelfHeal => plugin_host::self_heal(),
        Command::Complete => cmd_complete(),
        Command::ApiKey { profile } => cmd_api_key(&profile),
        Command::Completions { target, shell } => cmd_completions(&target, shell.as_deref()),
        Command::Herdr { cmd } => cmd_herdr(cmd),
        Command::Run { .. } => cmd_run(),
        Command::External(words) => cmd_external(&words),
    }
}

fn cmd_daemon(standby: bool, replace: bool, status: bool) -> Result<()> {
    if status {
        daemon::status_probe()
    } else if replace {
        daemon::serve(daemon::StartMode::Replace)
    } else if standby {
        daemon::serve(daemon::StartMode::Standby)
    } else {
        daemon::serve(daemon::StartMode::ExitIfRunning)
    }
}

fn cmd_complete() -> Result<()> {
    completions::print_profile_names();
    Ok(())
}

fn cmd_herdr(cmd: cli::HerdrCommand) -> Result<()> {
    match cmd {
        cli::HerdrCommand::Install {
            key,
            no_config,
            yes,
        } => {
            // The knob lives in profiles.toml (`AppState.herdr`), so the
            // row the plugin writes matches the TUI's setting. A missing
            // or unreadable file answers the default, never an error.
            let delegate_row_text = crate::profile::load_config()
                .map(|config| config.state.herdr.delegate_row_text)
                .unwrap_or_else(|_| crate::profile::HerdrSettings::default().delegate_row_text);
            herdr::install(key.as_deref(), no_config, yes, delegate_row_text)
        }
        cli::HerdrCommand::Uninstall { no_config, yes } => herdr::uninstall(no_config, yes),
        cli::HerdrCommand::Config { cmd } => match cmd {
            cli::HerdrConfigCommand::Get { key } => herdr::config_get(&key),
        },
    }
}

fn cmd_run() -> Result<()> {
    anyhow::bail!(
        "`clauth run` isn't a command; for a headless delegate use \
         `clauth start <profile> -p \"<prompt>\"` (or the MCP `delegate` tool)"
    )
}

// A bare word is a profile name. More than one word is nothing clauth
// knows: a usage error rather than the old help-and-exit-0, so a typo
// is distinguishable from success to a calling script.
fn cmd_external(words: &[String]) -> Result<()> {
    match words {
        [name] => cmd_switch(name),
        _ => Err(usage_error(format!(
            "unrecognized command '{}'; run `clauth --help` for the command list",
            words.join(" ")
        ))),
    }
}

/// `clauth completions <bash|zsh|fish>` prints a script; `clauth completions
/// install [shell]` writes it and wires it into the user's shell rc. Both live
/// under one subcommand with two positionals, so the second value is only
/// meaningful after `install`.
fn cmd_completions(target: &str, shell: Option<&str>) -> Result<()> {
    if target == "install" {
        return completions::install(shell);
    }
    if let Some(extra) = shell {
        return Err(usage_error(format!(
            "unexpected argument '{extra}'; `clauth completions {target}` takes no second value"
        )));
    }
    completions::print_script(target)
}

fn cmd_start(name: &str, rest: &[String], isolation: Isolation, follows_chain: bool) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    refuse_if_disabled(&config, &canonical)?;
    start::run(&config, &canonical, rest, isolation, None, follows_chain)
}

/// Where `clauth login <name>` lands. An EXISTING profile (matched
/// case-insensitively, carrying its stored canonical spelling) is
/// re-authenticated in place through the issue-#7 overwrite path; any other
/// name creates a fresh profile. Pure, so the routing is unit-testable without
/// `cmd_login`, which opens a real browser.
#[derive(Debug, PartialEq)]
enum LoginRoute {
    /// No profile has this name — mint tokens into a brand-new profile.
    New(String),
    /// A profile already exists under this (canonical) name — mint fresh
    /// tokens and overwrite its credential set in place, keeping its chain
    /// slot, env, and model settings.
    Reauth(String),
}

fn login_route(config: &AppConfig, raw: &str) -> LoginRoute {
    match config.canonical_name(raw.trim()) {
        // Route to the stored canonical spelling, not the typed case variant,
        // so `clauth login ACME` for stored `acme` refreshes the same profile
        // instead of bailing on the case-insensitive collision check.
        Some(existing) => LoginRoute::Reauth(existing),
        // Store the TRIMMED name. Every later lookup (`canonical_name`,
        // `resolve_or_bail`, `switch`) matches without trimming, so a padded
        // `"  new  "` would be unreachable afterwards and a later `login "new"`
        // wouldn't detect the collision, silently making a near-duplicate.
        None => LoginRoute::New(raw.trim().to_string()),
    }
}

/// Parse a reauth-overwrite confirmation with a default-NO policy — the op
/// replaces a profile's stored credentials, so silence must not proceed. Only
/// `y`/`yes` (case-insensitive) confirms.
fn reauth_confirmed(input: &str) -> bool {
    let a = input.trim();
    a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// Prompt `[y/N]` before a reauth overwrites a profile's stored credentials.
/// Non-TTY stdin proceeds (a piped script can't be prompted), matching the
/// OAuth reauth contract. `is_api` tailors the copy (endpoint + key vs tokens).
fn confirm_reauth(target: &str, is_api: bool) -> Result<bool> {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(true);
    }
    let object = if is_api {
        "endpoint + API key"
    } else {
        "stored credentials"
    };
    out!(
        "clauth: profile '{target}' already exists. Re-authenticating replaces its {object}. Continue? [y/N] "
    );
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(reauth_confirmed(&answer))
}

/// Collect the base_url + api_key pair for API-key mode. Each value comes from
/// its `--flag` when given; otherwise a prompt: base_url on a normal echo'ing
/// line, api_key echo-off (it's a secret). A non-TTY stdin that still owes a
/// value bails — a script must pass both flags explicitly.
fn collect_api_endpoint(
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal();

    let base_url = match base_url {
        // A flag value gets the same trim + empty-reject as the prompt, so
        // `--base-url ""` or a space-padded value can't slip through unvalidated.
        Some(u) => {
            let u = u.trim();
            if u.is_empty() {
                anyhow::bail!("base url is required for an API account");
            }
            Some(u.to_string())
        }
        None => {
            if !interactive {
                anyhow::bail!("non-interactive stdin: pass --base-url (and --api-key) explicitly");
            }
            out!("Base URL: ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let trimmed = line.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("base url is required for an API account");
            }
            Some(trimmed)
        }
    };

    let api_key = match api_key {
        Some(k) => {
            let k = k.trim();
            if k.is_empty() {
                anyhow::bail!("api key is required for an API account");
            }
            claude::validate_api_key(k)?;
            errln!(
                "clauth: warning: --api-key is visible in shell history and process listings; prefer the prompt"
            );
            Some(k.to_string())
        }
        None => {
            if !interactive {
                anyhow::bail!("non-interactive stdin: pass --api-key explicitly");
            }
            let k = rpassword::prompt_password("API key: ")
                .map_err(|e| anyhow::anyhow!("failed to read API key: {e}"))?;
            let k = k.trim().to_string();
            if k.is_empty() {
                anyhow::bail!("api key is required for an API account");
            }
            claude::validate_api_key(&k)?;
            Some(k)
        }
    };

    Ok((base_url, api_key))
}

/// Run the browser OAuth flow (preamble, authorize-URL paste fallback, minted
/// tokens, login summary, identity-anchor seed) and wrap it in a capture
/// snapshot. Shared by `cmd_login`'s new and reauth OAuth arms so the two stay
/// in lockstep.
fn run_oauth_browser(reauth: bool, target: &str) -> Result<actions::CaptureSnapshot> {
    if reauth {
        outln!("clauth: re-authenticating existing profile '{target}', opening a browser…");
    } else {
        outln!("clauth: opening a browser to log in to a new account for '{target}'…");
    }
    let outcome = oauth_login::login_with(|progress| {
        // The CLI surfaces only the paste-fallback URL; the later milestones
        // are TUI-modal fodder and would just be noise between the prints here.
        if let oauth_login::LoginProgress::AuthorizeUrl(url) = progress {
            outln!("\nIf the browser didn't open, visit this URL to authorize:\n{url}\n");
        }
    })
    // CLI stderr: name the HTTP status too. This lands on the `errln!`
    // backstop below, a terminal with no companion log open, and a fresh login
    // failing on a 400 is the case that ruling exists for.
    .map_err(|e| anyhow::anyhow!("{}", e.cli_message()))?;
    outln!(
        "clauth: login complete.\n{}",
        oauth_login::login_summary(&outcome.credentials)
    );
    // The uuid the login's own verification probe saw rides the snapshot to
    // whichever action commits it, which is what anchors the profile — so
    // `oauth::try_adopt_live_rotation` can prove a diverged live login is the
    // SAME account even after the stored token dies. Seeding it here instead
    // would anchor an account whose commit may still fail.
    Ok(actions::CaptureSnapshot {
        credentials: Some(outcome.credentials),
        base_url: None,
        api_key: None,
        account_uuid: outcome.account_uuid,
    })
}

/// `clauth login <name> [--base-url <url>] [--api-key <key>] [--model <id>]` —
/// add a new account or re-authenticate an existing one in place (#7). The auth
/// method is flag-selected: bare (no `--base-url`/`--api-key`) runs the browser
/// OAuth flow (`oauth_login`) and writes the minted tokens straight into the
/// profile's `.credentials.json`, identically on every platform; passing either
/// endpoint flag switches to API-key mode and captures a base_url + api_key pair
/// instead, prompting (echo-off for the key) for whatever a flag omitted.
///
/// A NEW name captures into a fresh profile; an EXISTING name routes through
/// [`actions::overwrite_captured_profile`] — the fresh credential set (tokens OR
/// endpoint + key) replaces the old in place (chain slot, env, and model
/// settings survive; stale per-account fetch caches are dropped; when it is the
/// ACTIVE profile the live link is re-run so a running `claude` picks the new
/// login up). A reauth that crosses types (OAuth ↔ API) is allowed: the
/// snapshot overwrites all three of credentials/base_url/api_key, so the old
/// type's leftovers are cleared. Neither path switches to the profile (`clauth
/// <name>` does that). `--model` is persisted onto the profile after capture.
/// Tokens are never printed — only a sha256 prefix.
fn cmd_login(args: LoginArgs) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let route = login_route(&config, &args.profile);
    let target = ProfileName::from(match &route {
        LoginRoute::Reauth(existing) => existing.clone(),
        LoginRoute::New(fresh) => {
            actions::validate_profile_name(fresh, crate::harness::Harness::Claude, None)?;
            fresh.clone()
        }
    });
    let reauth = matches!(route, LoginRoute::Reauth(_));
    let is_api = args.is_api_mode();

    // CLA-SPLIT capture flow: `--setup-token` writes the profile's
    // session-token sidecar and touches NOTHING else — the usage OAuth pair,
    // env, chain slot, and model settings all survive, so it needs none of
    // the reauth/overwrite machinery below.
    if args.setup_token {
        return cmd_login_setup_token(
            &mut config,
            &target,
            reauth,
            args.model.as_deref(),
            args.yes,
        );
    }

    // An Alibaba Model Studio profile's usage credential is a console session,
    // not an OAuth pair: the api key can't read quota and an Anthropic login
    // would mint tokens that endpoint has no use for. So a bare `clauth login`
    // on one runs the console flow instead. Deliberately ahead of
    // `confirm_reauth`: the session cannot be refreshed and lapses on a clock
    // clauth does not own, so re-running this login is the routine repair
    // rather than an overwrite to guard — and it replaces the console session
    // and NOTHING else. Not the api key: the callback returns a workspace key
    // for a different product, and `actions::store_console_login` exists to
    // keep it off the profile.
    if !is_api
        && reauth
        && config.find(&target).and_then(|p| p.provider) == Some(providers::Provider::Alibaba)
    {
        return cmd_login_console(&mut config, &target, args.model.as_deref());
    }

    // Confirm a reauth BEFORE collecting anything (browser or key prompt): a
    // declined overwrite must not open a browser or read a secret.
    if reauth && !confirm_reauth(&target, is_api)? {
        outln!("clauth: aborted. '{target}' left unchanged.");
        return Ok(());
    }

    if reauth {
        let snapshot = if is_api {
            let (base_url, api_key) =
                collect_api_endpoint(args.base_url.as_deref(), args.api_key.as_deref())?;
            actions::CaptureSnapshot {
                credentials: None,
                base_url,
                api_key,
                // An api-key login authenticates no Anthropic account.
                account_uuid: None,
            }
        } else {
            run_oauth_browser(true, &target)?
        };
        actions::overwrite_captured_profile(&mut config, &target, snapshot)?;
        // On a reauth `--model` is an explicit override; without it the
        // profile's existing model settings survive.
        if let Some(model) = args.model.as_deref() {
            actions::set_profile_default_model(&mut config, &target, model)?;
        }
        let what = if is_api { "endpoint + key" } else { "tokens" };
        outln!("clauth: re-authenticated '{target}'. Fresh {what} are in place.");
    } else if is_api {
        // A new API profile goes through `create_blank_profile` (the TUI's
        // path), NOT `capture_into_profile`: the latter auto-activates the
        // first profile and links credentials, but an API account carries no
        // credentials.json and its base_url/api_key reach the live
        // settings.json only via a switch — so auto-activating would mark it
        // "active" before it's wired. The user switches explicitly (the print
        // below), which writes settings.json. `create_blank_profile` also
        // takes the model inline, so no separate model write is needed here.
        let (base_url, api_key) =
            collect_api_endpoint(args.base_url.as_deref(), args.api_key.as_deref())?;
        actions::create_blank_profile(
            &mut config,
            target.to_string(),
            base_url,
            api_key,
            args.model.clone(),
        )?;
        outln!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    } else {
        let snapshot = run_oauth_browser(false, &target)?;
        actions::capture_into_profile(&mut config, target.to_string(), snapshot)?;
        // Apply the requested default model so the captured profile's sessions
        // route there from the first launch.
        if let Some(model) = args.model.as_deref() {
            actions::set_profile_default_model(&mut config, &target, model)?;
        }
        outln!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    }
    // CLA-SPLIT: the sidecar outranks `credentials.json` at every switch, so a
    // fresh OAuth login reaches usage polling and NOTHING ELSE while it exists.
    // Silence here reads as "the re-login took", which is what sent an operator
    // round the loop of re-authenticating an account that kept running on its
    // year-old mint.
    if !is_api && claude::has_session_token(&target) {
        outln!(
            "clauth: NOTE '{target}' still holds a long-lived token, and that is what switches \
             install. This login only feeds usage polling. Drop it with:  clauth static-token \
             {target} --clear"
        );
    }
    Ok(())
}

/// The Alibaba arm of [`cmd_login`]: open the Model Studio console, catch the
/// loopback callback, and store the session on the profile. The console front
/// and region come from the profile's own `base_url`, so nothing is prompted
/// for. The profile's api key is left alone — the callback's is workspace-scoped
/// and belongs to a different product (see `actions::store_console_login`).
fn cmd_login_console(config: &mut AppConfig, target: &str, model: Option<&str>) -> Result<()> {
    let target = ProfileName::from(target);
    let base_url = config
        .find(&target)
        .and_then(|p| p.base_url.clone())
        .unwrap_or_default();
    let (site, region) = providers::alibaba::site_and_region(&base_url)
        .context("this profile's endpoint is not an Alibaba Model Studio one")?;
    outln!(
        "clauth: opening the Alibaba Model Studio console to capture a usage session for '{target}'…"
    );
    let outcome = alibaba_login::login_with(site, region, |url| {
        outln!("\nIf the browser didn't open, visit this URL to sign in:\n{url}\n");
    })?;
    actions::store_console_login(config, &target, outcome.console.clone())?;
    if let Some(model) = model {
        actions::set_profile_default_model(config, &target, model)?;
    }
    outln!(
        "clauth: console session captured for '{target}'.\n{}",
        alibaba_login::login_summary(&outcome)
    );
    Ok(())
}

/// `clauth login <name> --setup-token [--yes] [--model <id>]` — capture a
/// `claude setup-token` mint into the profile's `session-token.json` sidecar
/// (CLA-SPLIT), replacing today's fill-it-by-hand step. The token is read
/// echo-off on a TTY (it's a bearer credential) or as one line from a piped
/// stdin (so a GUI/script can drive the capture); its value is never echoed
/// or logged. Additive: nothing else about the profile moves, and the sidecar
/// takes effect on the next switch — this deliberately does not touch the
/// live slot, so capturing can never sign a running session out.
///
/// It deliberately seeds no identity anchor either: the capture is an offline
/// paste (no network call), a setup token is the session bearer rather than an
/// OAuth pair, and `try_adopt_live_rotation` short-circuits for a session-token
/// profile, so there is nothing for the anchor to guard. A later
/// `clauth login <name>` that adds an OAuth pair seeds it then.
fn cmd_login_setup_token(
    config: &mut profile::AppConfig,
    target: &str,
    exists: bool,
    model: Option<&str>,
    yes: bool,
) -> Result<()> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal();
    let target = ProfileName::from(target);

    // Replacing an existing sidecar re-points every future switch at the new
    // token — confirm like the other in-place replacements. A fresh capture
    // (no sidecar yet) is additive and needs no ceremony.
    if claude::session_token_status(&target).is_some() && !yes {
        if !interactive {
            anyhow::bail!(
                "'{target}' already has a long-lived token; pass --yes to replace it non-interactively"
            );
        }
        out!("Replace the stored long-lived token for '{target}'? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            outln!("clauth: aborted. '{target}' left unchanged.");
            return Ok(());
        }
    }

    let raw = if interactive {
        outln!("clauth: capturing a long-lived token for '{target}'.");
        outln!("  1. in another terminal, run:  claude setup-token");
        outln!("  2. complete the browser flow it opens");
        outln!("  3. paste the minted token below (input stays hidden)");
        rpassword::prompt_password("Setup token: ")
            .map_err(|e| anyhow::anyhow!("failed to read the token: {e}"))?
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let token = claude::validate_setup_token(&raw)?;

    // A brand-new name gets a blank profile first (no credentials — the
    // sidecar IS its login; a usage OAuth pair can be added later with a
    // normal `clauth login`).
    if !exists {
        actions::create_blank_profile(
            config,
            target.to_string(),
            None,
            None,
            model.map(str::to_string),
        )?;
    } else if let Some(model) = model {
        actions::set_profile_default_model(config, &target, model)?;
    }

    // CLA-ROLL: on a rolling-token profile the next rotation overwrites this
    // mint with a rolling value — capture the mint into the sidecar AND the
    // degrade backup atomically (one flock section, same bytes; a two-step
    // write-then-copy can snapshot a concurrent rotation's rolling token as "the
    // mint").
    let rolling_on = config.find(&target).is_some_and(|p| p.rolling_token);
    let now = crate::usage::now_ms() as i64;
    let expires_at = if rolling_on {
        claude::write_session_token_with_backup(&target, &token, now)?
    } else {
        claude::write_session_token(&target, &token, now)?
    };
    let days = (expires_at - crate::usage::now_ms() as i64) / 86_400_000;
    outln!(
        "clauth: long-lived token installed for '{target}' · assumed to expire in ~{days}d \
         (`claude setup-token` mints last about a year)."
    );
    outln!(
        "clauth: it takes effect on the next switch:  clauth {target}{}",
        if exists {
            ""
        } else {
            "\nclauth: for usage polling, also add an OAuth pair later:  clauth login <name>"
        }
    );
    Ok(())
}

/// `clauth static-token <name> --clear [--yes]` — the exit from the long-lived
/// token entirely: it removes every piece of that state, so the profile's
/// stored OAuth pair is what switches install again and nothing re-creates a
/// sidecar behind the operator's back. Three pieces, each cleared when present:
///
/// - `session-token.json` — flips `install_source_path` back to
///   `credentials.json`.
/// - the `rolling_token` flag — FIRST, and load-bearing: with the flag still
///   set, the daemon's next scan re-stamps a fresh rolling bearer over the
///   removal, and the clear visibly "doesn't take" — the same daemon-fights-
///   operator failure the bare restore's flag-off-first ordering exists for.
/// - `session-token.static.json` — the preserved mint IS a long-lived
///   credential; "cleared" with a year-scale token still on disk would be
///   false. On a rolling profile it is the only long-lived piece, the
///   hours-scale bearer in the sidecar being the decoy.
///
/// A verb rather than a flag on `login`, matching how `enable`/`disable` toggle
/// per-profile state: this removes a credential, so hanging it off the command
/// that ADDS one reads as a login that does not log in. The bare verb is the
/// RESTORE (the inverse of `rolling-token`), which is why `--yes` requires
/// `--clear`.
///
/// Unlike the capture it CANNOT leave the live slot alone. The slot is a symlink
/// into the profile store, so removing its target under it leaves a dangling
/// link and Claude Code reads no credentials at all. An active profile is
/// therefore relinked in the same operation, which on Linux a live session picks
/// up on the next mtime move.
///
/// Refuses when the profile has no other login to fall back to (a name created
/// by `--setup-token` alone stores no OAuth pair): clearing there would strip its
/// only credential. `--yes` skips the confirm, never that guard.
///
/// "Other login" includes an API KEY, so the fall-back is not always an OAuth
/// pair and the copy branches on which it is (`claude::has_stored_oauth_login`).
/// An api-key profile clears onto an absent install source, where the relink
/// removes the live slot and, on macOS, signs the Keychain out — correct, since
/// an active api-key profile must not leave an Anthropic login serving, but the
/// opposite of the relink every line here used to promise.
fn cmd_static_token_clear(name: &str, yes: bool) -> Result<()> {
    use std::io::IsTerminal as _;
    platform::init();

    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    let target = &canonical;
    let profile = config
        .find(target)
        .ok_or_else(|| anyhow::anyhow!("no profile named '{target}'"))?;
    let sidecar_present = claude::session_token_status(target).is_some();
    let backup_present = claude::has_static_backup(target);
    let rolling_armed = profile.rolling_token;
    // "Nothing to clear" only when ALL THREE pieces are absent. A set flag with
    // no sidecar (the daemon just hasn't stamped yet, or someone removed the
    // file by hand) is exactly the state where an early "nothing to clear"
    // would leave the daemon to re-create what the operator was told is gone.
    if !sidecar_present && !backup_present && !rolling_armed {
        outln!("clauth: '{target}' holds no long-lived token, so nothing to clear.");
        return Ok(());
    }
    // The other-login guard covers the backup slot too: a preserved mint is
    // restorable by the bare verb, so destroying it when it is the profile's
    // only credential strips the profile the same way removing the sidecar
    // would.
    if (sidecar_present || backup_present)
        && profile.credentials.is_none()
        && profile.api_key.is_none()
    {
        anyhow::bail!(
            "'{target}' stores no other login, so clearing its long-lived token would leave it \
             with no credentials at all. Run `clauth login {target}` first, then clear."
        );
    }

    let active = config.is_active(target);
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!(
                "pass --yes to clear the long-lived token for '{target}' non-interactively"
            );
        }
        // What the confirm promises has to be what happens, so it reads the same
        // fact the relink below branches on. An api-key profile reaches here with
        // nothing to relink ONTO, and telling it a login comes back is the one
        // wrong thing to say before an irreversible prompt.
        if active {
            if claude::has_stored_oauth_login(target) {
                outln!(
                    "clauth: '{target}' is active — clearing relinks the live credentials onto \
                     its stored OAuth login, and running sessions follow."
                );
            } else {
                outln!(
                    "clauth: '{target}' is active and stores no OAuth login — clearing signs \
                     Claude Code out, leaving '{target}' on its api key, and running sessions \
                     follow."
                );
            }
        }
        if rolling_armed {
            outln!("{}", clear_disarm_note(target));
        }
        if backup_present {
            outln!("{}", clear_backup_note(target));
        }
        out!("Clear the long-lived token for '{target}'? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            outln!("clauth: aborted. '{target}' left unchanged.");
            return Ok(());
        }
    }

    // Serialized on the rotation guard for the same reason the bare restore is:
    // a rotation in flight that still sees the flag set would re-stamp the
    // sidecar AFTER the removal below, resurrecting the very state this command
    // just reported cleared.
    let _guard = runtime::RotationGuard::acquire(target)
        .with_context(|| format!("could not lock '{target}' to clear its long-lived token"))?;
    // Everything the ACTIONS key off is re-read under the guard; the snapshot
    // above fed the prompt's copy and nothing else. The prompt is an unbounded
    // wait, and a rotation, a switch, or a re-login can all land during it —
    // `save_profile` persists the WHOLE profile, so writing the pre-prompt
    // snapshot back would rewind `credentials.json` to a spent refresh token
    // (the exact hazard `adopt_disk_rotation` names, one layer up), and acting
    // on the pre-prompt `active` relinks the wrong world in both directions.
    let mut on_disk = profile::load_profile(target)?;
    // The other-login REFUSAL re-checks here too — it is the one decision in
    // this function that can strip a profile of its last credential, and the
    // prompt above is an unbounded wait a log-out can land inside: a profile
    // that stored an OAuth pair when the prompt was printed may store nothing
    // by the time the operator confirms.
    if (claude::session_token_status(target).is_some() || claude::has_static_backup(target))
        && on_disk.credentials.is_none()
        && on_disk.api_key.is_none()
    {
        anyhow::bail!(
            "'{target}' stores no other login anymore, so clearing its long-lived token would \
             leave it with no credentials at all. Run `clauth login {target}` first, then clear."
        );
    }
    let rolling_armed = on_disk.rolling_token;
    if rolling_armed {
        on_disk.rolling_token = false;
        profile::save_profile(&on_disk)?;
    }
    // A mis-filled sidecar is EVIDENCE — the anomaly the split exists to
    // detect, and one the operator did not name: the prompt says "the
    // long-lived token", and a rotating pair is precisely not that. Every
    // other removal of a mis-fill moves it aside first; this one does too,
    // and the plain-delete argument on `clear_static_backup` stays scoped to
    // the operator's own mint.
    claude::quarantine_misfilled_sidecar(target)?;
    claude::clear_session_token(target)?;
    let active = load_config()?.is_active(target);
    // Re-read AFTER the clear: `install_source_path` only falls back to
    // `credentials.json` once the sidecar is gone, so this is the store the
    // relink below actually finds.
    let has_login = claude::has_stored_oauth_login(target);
    if active {
        claude::force_link_profile_credentials(target)?;
        if has_login {
            outln!(
                "clauth: cleared the long-lived token for '{target}' and relinked the live \
                 credentials onto its stored OAuth login."
            );
        } else if on_disk.api_key.is_some() {
            outln!(
                "clauth: cleared the long-lived token for '{target}' and signed Claude Code \
                 out: it stores no OAuth login, so '{target}' authenticates by api key."
            );
        } else {
            // Reachable only on the flag-only widening (a stored piece with no
            // other login is refused above): claiming an api key the profile
            // does not hold would be this line's own lie.
            outln!(
                "clauth: cleared the long-lived token for '{target}' and signed Claude Code \
                 out: it stores no login at all now — run `clauth login {target}` before \
                 switching to it."
            );
        }
    } else if has_login {
        outln!(
            "clauth: cleared the long-lived token for '{target}'. Its stored OAuth login \
             installs on the next switch:  clauth {target}"
        );
    } else if on_disk.api_key.is_some() {
        outln!(
            "clauth: cleared the long-lived token for '{target}'. It stores no OAuth login, \
             so switching to it authenticates by api key:  clauth {target}"
        );
    } else {
        outln!(
            "clauth: cleared the long-lived token for '{target}'. It stores no login at all \
             now — run `clauth login {target}` before switching to it."
        );
    }
    // The backup goes LAST, after the relink: failing between the sidecar
    // removal and the relink would leave an active profile's live slot a
    // dangling symlink under a bare "remove failed" — a broken login reported
    // as nothing-happened. From here the live slot is already sound, so a
    // failed removal is exactly what its context says and no more.
    let backup_removed = claude::clear_static_backup(target).with_context(|| {
        format!(
            "the long-lived token for '{target}' is cleared, but the preserved mint at \
             session-token.static.json remains — remove it by hand, or re-run"
        )
    })?;
    for line in clear_postscripts(target, rolling_armed, backup_removed) {
        outln!("{line}");
    }
    Ok(())
}

/// The post-clear report, composed as a value so its GATING is pinnable: there
/// is no stdout capture in this crate by construction, so a test cannot see
/// what the command printed — but it can see what this returns for each state,
/// and the single caller keeps a deleted print a dead-code error.
fn clear_postscripts(target: &str, rolling_armed: bool, backup_removed: bool) -> Vec<String> {
    let mut lines = Vec::new();
    if rolling_armed {
        lines.push(clear_disarmed_postscript(target));
    }
    if backup_removed {
        lines.push(clear_backup_postscript(target));
    }
    lines
}

/// The pre-prompt warning that a rolling profile's re-stamping stops with the
/// clear. Single-caller fns, like the arm-report copy and for the same reason:
/// there is no stdout capture, so the unit pin is on content and a deleted
/// print orphans the fn into a dead-code error under `-D warnings`.
fn clear_disarm_note(target: &str) -> String {
    format!("clauth: '{target}' is on the rolling token — clearing also turns its re-stamping off.")
}

/// The pre-prompt warning that the preserved mint goes with the clear — the
/// only line telling the operator a SECOND, restorable credential is about to
/// be destroyed, which makes it a posture line by the same argument as
/// [`scope_widening_disclosure`].
fn clear_backup_note(target: &str) -> String {
    format!(
        "clauth: the preserved mint at session-token.static.json goes with it — \
         `clauth static-token {target}` will have nothing to restore."
    )
}

/// The post-clear statement that nothing re-stamps a sidecar anymore — the
/// disarm half of the report, and the only confirmation the flag actually
/// moved.
fn clear_disarmed_postscript(target: &str) -> String {
    format!("clauth: rolling-token is off · nothing re-stamps a sidecar for '{target}' now.")
}

/// The post-clear statement that the preserved mint is destroyed. UNCONDITIONAL
/// on the removal, unlike [`clear_backup_note`], which prints only on the
/// interactive path: a non-TTY stdin refuses without `--yes`, so every scripted
/// clear skips the prompt copy entirely — and a year-scale credential removed
/// with nothing saying so is exactly the silence the disarm postscript already
/// refuses for the flag.
fn clear_backup_postscript(target: &str) -> String {
    format!(
        "clauth: the preserved mint at session-token.static.json is gone · \
         `clauth static-token {target}` has nothing to restore now."
    )
}

/// `clauth delete <name> [--yes] [--force]` — remove a profile and all its
/// credentials (the whole on-disk profile dir + state + caches), OAuth or
/// API-key. Prompts `[y/N]` on a TTY unless `--yes`. Delete is an irreversible
/// `remove_dir_all`, so unlike a reauth a non-TTY stdin does NOT get an implicit
/// yes: it must pass `--yes`, else the delete is refused. A profile held by a
/// live `clauth start` session is refused unless `--force` (independent of
/// `--yes`). If the deleted profile was active, its live
/// `~/.claude/.credentials.json` link and settings.json endpoint are cleared.
fn cmd_delete(name: &str, yes: bool, force: bool) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    if !yes {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "refusing to delete '{canonical}' without confirmation; pass --yes for a non-interactive delete"
            );
        }
        out!("clauth: delete profile '{canonical}' and all its credentials? [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            outln!("clauth: aborted. '{canonical}' left in place.");
            return Ok(());
        }
    }
    let was_active = config.is_active(&canonical);
    let rotation = actions::rotation_guard_for_mutation(&canonical)?;
    actions::delete_profile(&mut config, &canonical, force, &rotation)?;
    if was_active {
        outln!("clauth: deleted profile '{canonical}' (was active; live credentials cleared).");
    } else {
        outln!("clauth: deleted profile '{canonical}'.");
    }
    Ok(())
}

/// Refuse `name` as a switch/start target when it's user-disabled, naming the
/// fix. Called by [`cmd_switch`] and [`cmd_start`] as a friendly early check,
/// and by `start::run` — the authoritative chokepoint every session-spawn path
/// (`cmd_start`, `sessions_cli::run_resume`) funnels through — so all three
/// callers share one message instead of drifting.
fn refuse_if_disabled(config: &AppConfig, name: &ProfileName) -> Result<()> {
    if config.find(name).is_some_and(|p| p.is_disabled()) {
        anyhow::bail!("'{name}': account is disabled, run `clauth enable {name}`");
    }
    Ok(())
}

/// `clauth disable <name> [--yes|-y]` — mark `name` as user-disabled
/// ([`actions::disable_profile`]): invisible to the fallback chain, the usage
/// scheduler, and the daemon status feed by default, while its dir and
/// credentials stay on disk untouched. Refuses when `name` is the active
/// profile or holds a live `clauth start` session (each names its own
/// blocker). Prompts `[y/N]` on a TTY unless `--yes`; a non-TTY stdin must
/// pass `--yes`, mirroring [`cmd_delete`]'s confirm policy. Already-disabled
/// is a no-op — reported, not refused, and never prompted.
fn cmd_disable(name: &str, yes: bool) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;

    if config.find(&canonical).is_some_and(|p| p.is_disabled()) {
        outln!("clauth: '{canonical}' is already disabled.");
        return Ok(());
    }

    if !yes {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "refusing to disable '{canonical}' without confirmation; pass --yes for a non-interactive run"
            );
        }
        out!(
            "clauth: disable profile '{canonical}'? it drops out of auto-switch and usage \
             polling until re-enabled. [y/N] "
        );
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            outln!("clauth: aborted. '{canonical}' left unchanged.");
            return Ok(());
        }
    }

    actions::disable_profile(&mut config, &canonical)?;
    outln!("clauth: disabled '{canonical}'.");
    Ok(())
}

/// `clauth enable <name>` — clear `name`'s disabled flag
/// ([`actions::enable_profile`]), restoring it to every operational surface.
/// No other side effects. Already-enabled is a no-op — reported, not refused.
fn cmd_enable(name: &str) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    if actions::enable_profile(&mut config, &canonical)? {
        outln!("clauth: enabled '{canonical}'.");
    } else {
        outln!("clauth: '{canonical}' is already enabled.");
    }
    Ok(())
}

fn cmd_switch(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    refuse_if_disabled(&config, &canonical)?;
    actions::switch_profile_cli(config, &canonical)
}

/// `clauth rolling-token <profile>` — serve this profile's sessions a bearer
/// re-stamped from its own usage chain.
///
/// Flips the flag, pre-clears a mis-filled sidecar (quarantining the evidence
/// first), then arms the sidecar through the same decision table the switch-in
/// gate uses. Reinstalls live when the profile is active, so a running `claude`
/// picks the new bearer up on its next request.
fn cmd_rolling_token(name: &str) -> Result<()> {
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    // Same gate `start` and `switch` take. A disabled profile is off every
    // operational surface, the re-stamp timer included, so arming one produces
    // a bearer that dies in hours with nothing behind it.
    refuse_if_disabled(&config, &canonical)?;
    // Unreachable by contract — `resolve_or_bail` just proved membership — so
    // this arm is a defensive seam, not a user-facing path (the user-facing
    // not-found already exited 2 above).
    let Some(profile) = config.find(&canonical) else {
        anyhow::bail!("profile '{canonical}' vanished between resolve and read");
    };

    let Some(oauth) = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
    else {
        anyhow::bail!(
            "'{canonical}' has no usage OAuth chain to roll from; run \
             `clauth login {canonical}` first"
        );
    };
    // A standing quarantine is checked BEFORE anything destructive runs: the
    // arm below would only degrade-and-bail on a dead chain, and by then the
    // mis-fill pre-clear would have removed the sidecar — leaving the profile
    // with nothing at all, which is worse than the disengaged mis-fill it
    // started in.
    if config.state.auth_broken.contains(&canonical) {
        anyhow::bail!(
            "'{canonical}' usage chain is dead · run `clauth login {canonical}` first, \
             then re-run"
        );
    }
    // A chain captured without `user:profile`/`subscriptionType` mints bearers
    // that still authenticate but may not unlock plan-gated models — warn
    // rather than refuse, since the roll is otherwise correct.
    let plan_capable = oauth
        .scopes
        .as_ref()
        .is_some_and(|s| s.iter().any(|x| x == "user:profile"))
        && oauth.subscription_type.is_some();
    if !plan_capable {
        outln!(
            "clauth: warning: the usage chain for '{canonical}' is missing the user:profile \
             scope or a subscriptionType stamp · rolling tokens may not unlock plan-gated \
             models. A fresh `clauth login {canonical}` browser sign-in fixes that."
        );
    }
    // A mis-filled sidecar is pre-cleared here, where overwriting is explicit
    // operator intent — the evidence still goes to quarantine first.
    if claude::quarantine_misfilled_sidecar(&canonical)? {
        outln!(
            "clauth: '{canonical}' had a mis-filled sidecar (a rotating pair). It was \
             quarantined under the profile's quarantine/ dir before arming."
        );
    }
    let is_active = config.is_active(&canonical);
    let was_rolling = profile.rolling_token;
    let handle: profile::ConfigHandle =
        std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
    // The flag persists BEFORE the arm, then rolls back to its PRIOR value on
    // a failed report. Persist-after looked cleaner ("nothing durable on
    // failure") and was wrong twice over: the gate's refresh leg stamps the
    // sidecar through the rotation hook, and that hook is FLAG-GATED — arming
    // with the flag still false made the very refresh the arm triggered stamp
    // nothing — and a reader walking away from `clauth rolling-token | head`
    // exits the process at the report, which would have left an armed rolling
    // bearer with no flag and nothing ever re-stamping it. Rollback-on-bail
    // keeps both invariants: the arm runs with the flag it needs, and a
    // non-zero exit from the arm leaves the flag exactly as it was found.
    // (After a SUCCESSFUL arm the set flag is the new truth — the live-install
    // step at the bottom can still exit non-zero, correctly leaving it on.)
    if let Ok(mut cfg) = handle.lock()
        && let Some(profile) = cfg.find_mut(&canonical)
    {
        profile.rolling_token = true;
        profile::save_profile(profile)?;
    }
    let armed =
        oauth::arm_rolling_token(&handle, &canonical, oauth::refresh_result).and_then(|()| {
            // Re-read under the handle: the gate marks a terminally dead chain
            // `auth_broken` on its way to degrading, and that is what separates
            // "nothing to retry" from "the daemon will get it".
            let chain_is_broken = handle
                .lock()
                .ok()
                .is_some_and(|c| c.state.auth_broken.contains(&canonical.as_str().into()));
            report_armed_sidecar(&canonical, chain_is_broken)
        });
    if let Err(e) = armed {
        if !was_rolling
            && let Ok(mut cfg) = handle.lock()
            && let Some(profile) = cfg.find_mut(&canonical)
        {
            profile.rolling_token = false;
            // The arming error is the one the operator needs; a rollback that
            // then cannot save its own config write must not replace it.
            if let Err(save_err) = profile::save_profile(profile) {
                errln!("{}", rollback_stranded_warning(&canonical, &save_err));
            }
        }
        return Err(e);
    }
    if is_active {
        claude::force_link_profile_credentials(&canonical)?;
        outln!("clauth: installed live. New sessions run on it immediately.");
    } else {
        outln!("clauth: it installs on the next switch:  clauth {canonical}");
    }
    Ok(())
}

/// The warning for a failed arm whose ROLLBACK also failed to save: the flag
/// is durably on with nothing armed, and this line is the only thing telling
/// the operator that state exists and how to leave it. A separate fn so the
/// copy is unit-pinned and a deleted print orphans it into a dead-code error —
/// the warning is best-effort by design (the arming error still propagates),
/// which is exactly what made it silently deletable.
fn rollback_stranded_warning(canonical: &str, save_err: &anyhow::Error) -> String {
    format!(
        "clauth: warning: could not roll the rolling-token flag back for \
         '{canonical}' ({save_err:#}) · once ~/.clauth is writable, run \
         `clauth static-token {canonical}` to clear it"
    )
}

/// The re-stamp promise `report_armed_sidecar` closes on — the DAEMON's to
/// keep, so each health state gets its own claim and none may stand in for
/// another: `Fresh` promising while no daemon runs is the arm reading as
/// durable when nothing will re-stamp it, and `Absent`'s warning on a healthy
/// daemon buries the one real signal in noise. A separate fn so the three
/// arms are unit-pinned as DISTINCT — measured interchangeable before.
fn restamp_promise(health: crate::daemon::DaemonHealth) -> &'static str {
    match health {
        // Best-effort probe: an error reads Absent by design, so the
        // claim is hedged rather than absolute.
        crate::daemon::DaemonHealth::Absent => {
            "No daemon appears to be running · nothing re-stamps it until \
             `clauth daemon` starts."
        }
        crate::daemon::DaemonHealth::Stale => {
            "A daemon is present but its feed looks stale · check \
             `clauth daemon --status` or re-stamps may not land."
        }
        crate::daemon::DaemonHealth::Fresh => "The daemon re-stamps it before it expires.",
    }
}

/// The security disclosure printed after a confirmed arm — the ENTIRE
/// user-facing statement that the session credential just got wider, and the
/// way back. There is deliberately no confirm prompt (a security prompt on a
/// repeatable command gets clicked through), so this line plus the SECURITY.md
/// row IS the feature's disclosure posture: a separate fn so the copy is
/// unit-pinned and a deleted print orphans it into a dead-code error, making
/// the one copy hole that would be a posture change a compile failure instead.
fn scope_widening_disclosure(canonical: &str) -> String {
    format!(
        "clauth: that is wider than the setup-token mint it supersedes, which carried \
         two. Anything that can read this profile's session credential can now use \
         every one of those scopes. `clauth static-token {canonical}` puts the mint \
         back."
    )
}

/// Say what sessions now hold, read back off disk rather than assumed.
///
/// The gate returns `Ready` on three paths that armed NOTHING and left the
/// preserved mint in place (dead chain, failed sidecar write with a live mint,
/// transient chain trouble with a live mint), and `has_session_token` is true
/// on all of them — so a flat "armed from the usage chain" line is a lie
/// exactly when the operator most needs the truth. `sidecar_kind` separates
/// them exactly.
///
/// No confirm prompt: a security prompt on a repeatable command gets clicked
/// through. The printed scope list plus the SECURITY.md row is the honest
/// middle, and it is printed AFTER the fact, when it describes something real.
fn report_armed_sidecar(canonical: &ProfileName, chain_is_broken: bool) -> Result<()> {
    let Some((kind, token)) = claude::sidecar_summary(canonical) else {
        // The gate said Ready and `has_session_token` held, yet the read-back
        // finds nothing parseable — a race or a filesystem fault. Claiming
        // "armed" here would be exactly the assumed-not-read report this
        // function's contract forbids.
        anyhow::bail!(
            "arming reported success but '{canonical}' has no readable sidecar to verify, \
             so nothing was confirmed · check permissions on ~/.clauth and re-run"
        );
    };
    match kind {
        // The gate returns `Ready` on three paths that armed NOTHING and left
        // the mint in place, and they do not deserve one message. A dead chain
        // is not something the daemon retries out of — it needs a re-login, and
        // a script has to be able to tell that from success.
        claude::SidecarKind::Mint if chain_is_broken => {
            anyhow::bail!(
                "'{canonical}' usage chain is dead, so sessions stay on the static mint and \
                 nothing would re-stamp a rolling token. Run `clauth login {canonical}` to \
                 revive the chain, then re-run. The rolling-token flag was left as it was."
            );
        }
        claude::SidecarKind::Mint => {
            outln!(
                "clauth: '{canonical}' usage chain could not be read just now, so sessions \
                 stay on the static mint. The flag is set · the daemon re-stamps on its next \
                 rotation."
            );
        }
        claude::SidecarKind::Misfilled => {
            // The CLI pre-cleared one before arming, so reaching this means a
            // writer raced a fresh rotating pair back into the sidecar.
            anyhow::bail!(
                "'{canonical}' sidecar was re-filled with a rotating pair while arming, so \
                 the split is disengaged and nothing was armed · re-run, and if it repeats \
                 find what is writing session-token.json"
            );
        }
        claude::SidecarKind::Rolling => {
            let scopes = token.scopes.clone().unwrap_or_default();
            let plan = token.subscription_type.clone().unwrap_or_default();
            // The re-stamp promise is the DAEMON's to keep, so it is only
            // made when one is actually there to keep it.
            let restamp = restamp_promise(crate::daemon::daemon_health());
            outln!(
                "clauth: rolling token armed for '{canonical}'. Sessions now hold the usage \
                 chain's access token: {} scope(s){}{}, and no refresh token. {restamp}",
                scopes.len(),
                if scopes.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", scopes.join(", "))
                },
                if plan.is_empty() {
                    String::new()
                } else {
                    format!(", plan {plan}")
                },
            );
            outln!("{}", scope_widening_disclosure(canonical));
        }
    }
    Ok(())
}

/// `clauth static-token <profile>` — put the preserved `claude setup-token`
/// mint back in front of sessions.
///
/// The inverse of `rolling-token`: flips the flag and restores the backup. Not
/// gated on the profile being enabled — walking a disabled profile back to a
/// mint that needs no re-stamping is always allowed.
fn cmd_static_token(name: &str) -> Result<()> {
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    // The whole restore (flag flip + mint restore) serializes on the profile's
    // rotation guard: without it, a concurrent rotation that still sees the
    // flag set can re-stamp the sidecar AFTER the restore, leaving the flag
    // off, an hours-horizon live credential, and no backup.
    // NOT a contention hint: `RotationGuard::acquire` ends in a BLOCKING
    // `File::lock()`, so a sibling refresh makes this WAIT rather than fail.
    // Arriving at the error means the lock file could not be created or opened
    // — a filesystem or permissions problem under `~/.clauth` — and the
    // original error says which, so it is kept rather than replaced.
    let _guard = runtime::RotationGuard::acquire(&canonical)
        .with_context(|| format!("could not lock '{canonical}' to restore its static token"))?;
    // Flag OFF first, before the restore is known to land — DELIBERATE, and
    // load-bearing for every bail arm below, none of which rolls it back. The
    // command's primary effect is the disarm: with the flag still on, the
    // daemon's next scan re-stamps a rolling bearer over the very mint the
    // failure copy tells the operator to capture, so their fresh
    // `--setup-token` login visibly "doesn't take". With it off, the
    // prescribed re-mint lands and stays. The cost is owned where it bites:
    // the rolling-bearer bail says in so many words that this command is what
    // stopped the re-stamping.
    //
    // Written from a fresh DISK read, never the pre-acquire config snapshot:
    // the blocking acquire above can WAIT out an in-flight rotation, and
    // `save_profile` persists the whole profile — writing the snapshot back
    // would rewind the very rotation it just waited for to a spent refresh
    // token.
    let mut on_disk = profile::load_profile(&canonical)?;
    if on_disk.rolling_token {
        on_disk.rolling_token = false;
        profile::save_profile(&on_disk)?;
    }
    let is_active = load_config()?.is_active(&canonical);
    // The flag flip above is already durable, so a restore that ERRORS (the
    // backup unreadable, the sidecar unwritable) must own that state the same
    // way the bail arms below do — a raw filesystem error here read as though
    // the command had done nothing.
    let restored = claude::restore_static_mint(&canonical).with_context(|| {
        format!(
            "'{canonical}' is off the rolling token now, but its preserved mint could not \
             be restored · fix the file problem, then re-run `clauth static-token {canonical}`"
        )
    })?;
    if restored {
        outln!("clauth: '{canonical}' is back on its static long-lived mint.");
        if is_active {
            claude::force_link_profile_credentials(&canonical)?;
            outln!("clauth: reinstalled live.");
        }
        return Ok(());
    }
    // Nothing was restored. What that MEANS depends on what the sidecar holds,
    // and the states do not deserve one verdict: a profile already on its mint
    // is a successful no-op, while a rolling bearer left with nothing
    // re-stamping it is a failed restore a script must see as non-zero.
    let backup_exists = profile::profile_dir(&canonical)?
        .join("session-token.static.json")
        .exists();
    // A backup that exists but did not restore is an EXPIRED backup
    // (`restore_static_mint` refuses to install a dead mint, and quarantines
    // away anything that is not a mint at all) — every FAILING verdict below
    // names it when present, or the operator hunts for a file that is right
    // there. The no-op success stays quiet: nothing about a dead backup
    // changes what that verdict reports.
    let backup_note = if backup_exists {
        " An expired preserved backup is on disk and was left in place."
    } else {
        ""
    };
    match claude::sidecar_summary(&canonical) {
        Some((claude::SidecarKind::Mint, token)) => {
            // The clock check the no-op verdict must make: "already on its
            // mint" is only a success while the mint is alive — an expired
            // one signs sessions out on the next switch, which is a failed
            // restore whatever the file layout says. Alive on the SAME grace
            // the restore rule uses (`BACKUP_EXPIRY_GRACE_MS`, Claude Code's
            // own five-minute refresh threshold): identical bytes must not
            // read as dead in the backup slot and fine in the live one.
            if token.expires_at.is_some_and(|exp| {
                crate::usage::now_ms() as i64 + claude::BACKUP_EXPIRY_GRACE_MS >= exp
            }) {
                anyhow::bail!(
                    "'{canonical}' is off the rolling token, but the static mint in its \
                     sidecar has EXPIRED (or sits inside Claude Code's own five-minute \
                     refresh window) and there is nothing to restore over it.{backup_note} \
                     Re-mint with `clauth login {canonical} --setup-token`."
                )
            }
            outln!(
                "clauth: '{canonical}' is already on its static long-lived mint · the \
                 rolling token is off. Nothing to restore."
            );
            Ok(())
        }
        Some((claude::SidecarKind::Rolling, _)) => {
            anyhow::bail!(
                "'{canonical}' is off the rolling token now, but there was no live static \
                 mint to restore.{backup_note} The last rolling bearer serves until it \
                 expires, and this command is what stopped its re-stamping · capture a \
                 fresh long-lived login with `clauth login {canonical} --setup-token`."
            )
        }
        Some((claude::SidecarKind::Misfilled, _)) => {
            anyhow::bail!(
                "'{canonical}' sidecar holds a rotating pair (mis-filled), so the split is \
                 disengaged and there is no live static mint to restore.{backup_note} \
                 Re-capture with `clauth login {canonical} --setup-token`."
            )
        }
        None => {
            anyhow::bail!(
                "'{canonical}' has no session-token sidecar and no live preserved mint · \
                 sessions run on the rotating pair.{backup_note} Capture a long-lived login \
                 with `clauth login {canonical} --setup-token`."
            )
        }
    }
}

/// `clauth __api-key <profile>` — the body CC's `apiKeyHelper` invokes per
/// request for an api-key profile. Loads the key from the profile's
/// `config.toml` (0o600) and prints it to stdout. The key never reaches argv
/// (the helper command line carries only the profile name) nor the spawned
/// CC process's env (the runtime `settings.json` writes `apiKeyHelper`, not
/// `env.ANTHROPIC_AUTH_TOKEN`). Fails closed with no stdout if the profile
/// is missing or carries no api_key, so a misconfigured helper surfaces as a
/// 401, not a silent leak of some other value.
fn cmd_api_key(name: &str) -> Result<()> {
    let key = api_key_for_profile(name)?;
    // `api_key_for_profile` returns Ok(Some) only when the key is non-empty;
    // Ok(None) means the profile has no key to mint, so the helper must fail
    // closed rather than emit a blank line CC would send as a credential.
    let Some(key) = key else {
        anyhow::bail!("profile '{name}' has no api_key");
    };
    let mut stdout = std::io::stdout().lock();
    write_api_key(&mut stdout, &key)
}

/// Write the api_key verbatim to `writer` — NO trailing newline, NO framing.
/// CC's `apiKeyHelper` contract does not document whether stdout is trimmed
/// (the docs say only "any shell command that prints the current credential
/// to stdout"), so the no-newline form strictly dominates: it is correct
/// whether CC trims or not, whereas `key + "\n"` would only be correct under
/// the unverified trim assumption. For a credential path, fail safe — CC
/// reads the bytes via EOF on process exit, no line-read hang.
fn write_api_key<W: std::io::Write>(writer: &mut W, key: &str) -> Result<()> {
    writer
        .write_all(key.as_bytes())
        .context("writing api_key to stdout")?;
    writer.flush().context("flushing api_key to stdout")
}

/// Read a profile's stored api_key from `config.toml`. Returns `Ok(None)` for
/// a profile that exists but has no api_key, `Err` for a missing profile or
/// unreadable config. Kept separate from [`cmd_api_key`] so the load is
/// unit-testable without capturing stdout. An empty key reads as `None`:
/// a credential that is whitespace-only is not a credential.
fn api_key_for_profile(name: &str) -> Result<Option<String>> {
    let name = ProfileName::from(name);
    // `load_profile` is permissive — a missing `config.toml` reads as the
    // default profile, so a helper pointing at a typo'd or deleted name would
    // otherwise return `Ok(None)` indistinguishable from a real no-key
    // profile. The dir-existence check fails closed with a clearer message
    // instead; both cases still surface as exit 1 via `cmd_api_key`.
    if !profile::profile_dir(&name)?.exists() {
        anyhow::bail!("profile '{name}' not found");
    }
    let profile = profile::load_profile(&name)?;
    let key = profile
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Fail closed on a hand-edited config that poisoned the key with control
    // chars: emitting it verbatim would inject a header, so refuse to mint.
    if let Some(k) = key {
        claude::validate_api_key(k)?;
    }
    Ok(key.map(str::to_string))
}

fn cmd_tui(theme_override: Option<tui::theme::Tier>) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
    completions::auto_install_once();
    let config = load_config()?;
    // Config-file tier: profiles.toml `theme = "full"|"compatible"`.
    // CLI flag beats config; both beat auto-detect.
    let config_tier = config.state.theme.map(|t| match t {
        ThemeName::Full => tui::theme::Tier::Full,
        ThemeName::Compatible => tui::theme::Tier::Compatible,
    });
    tui::theme::init(theme_override.or(config_tier));
    // herdr injects `HERDR_ENV=1` into every pane it manages, and only the
    // exact `"1"` counts (the same shape CLAUTH_NO_UPDATE reads). The settled
    // detection channel: no flag, no config key, so a normal terminal can
    // never trip it by accident.
    let herdr_mode = std::env::var("HERDR_ENV").as_deref() == Ok("1");
    tui::run(config, herdr_mode)
}

/// Feature→test traceability map.
#[cfg(test)]
#[path = "../tests/inline/feature_coverage.rs"]
mod feature_coverage;

#[cfg(test)]
#[path = "../tests/inline/cli.rs"]
mod tests;
