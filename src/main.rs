mod actions;
mod claude;
mod claude_json;
mod cli;
mod codex;
mod completions;
mod daemon;
mod doctor;
mod fallback;
mod fallback_config;
mod format;
mod jsonsync;
// macOS-only: Claude Code reads its login from the Keychain, not the credentials
// file, so a switch must also write there. Gated so non-macOS builds stay clean.
#[cfg(target_os = "macos")]
mod keychain;
mod lock;
mod lockorder;
mod logline;
mod loopback;
mod mcp;
mod oauth;
mod oauth_login;
mod platform;
mod plugin_probe;
mod poll;
mod pricing;
mod profile;
mod profile_cache;
mod profile_json;
mod providers;
mod proxy;
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
mod which;

#[cfg(test)]
mod testutil;

use anyhow::{Context, Result};
use clap::Parser as _;

use crate::cli::{Cli, Command, LoginArgs, ThemeArg};
use crate::profile::{AppConfig, ThemeName, load_config};
use crate::runtime::Isolation;

/// Resolve `name` to its canonical spelling, or bail with a [`UsageError`].
/// A bare unrecognized word lands here as a profile name (clap's `external`
/// subcommand), so a typo'd subcommand and a typo'd profile name are
/// indistinguishable at this position. Either way the caller named something
/// that isn't there: a usage error (exit 2), not a runtime failure (exit 1).
/// Shared by `start`/`delete`/`disable`/`enable`/`switch`.
fn resolve_or_bail(config: &AppConfig, name: &str) -> Result<String> {
    config.canonical_name(name).ok_or_else(|| {
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
            eprintln!("Error: {e:?}");
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
        Command::Start(a) => cmd_start(
            &a.profile,
            &a.claude_args,
            a.isolation(),
            a.rescue_override(),
        ),
        Command::Login(a) => cmd_login(a),
        Command::Delete {
            profile,
            yes,
            force,
        } => cmd_delete(&profile, yes, force),
        Command::Disable { profile, yes } => cmd_disable(&profile, yes),
        Command::Enable { profile } => cmd_enable(&profile),
        Command::Which { json } => which::run(json),
        Command::Sessions { json } => sessions_cli::run_sessions(json),
        // One verb, two meanings, split by what the argument names: a KNOWN
        // codex profile routes to the codex carryover (fork CDX-1c), anything
        // else is upstream's claude session resume (session id / `latest` —
        // session ids are UUIDs, so they can't collide with a profile name).
        // `--profile` forces the claude session path.
        Command::Resume { target, profile } => match profile {
            Some(profile) => sessions_cli::run_resume(&target, Some(&profile)),
            None => cmd_resume_dispatch(&target),
        },
        Command::Info { target } => sessions_cli::run_info(&target),
        Command::Daemon {
            standby,
            replace,
            status,
            // The default's explicit spelling: nothing to branch on.
            no_standby: _,
        } => {
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
        Command::Status {
            json: _,
            all,
            disabled,
        } => daemon::status_oneshot(all || disabled),
        Command::Mcp => mcp::serve(),
        Command::McpAwaitJob => mcp::await_job(),
        Command::Complete => {
            completions::print_profile_names();
            Ok(())
        }
        Command::ApiKey { profile } => cmd_api_key(&profile),
        Command::Completions { target, shell } => cmd_completions(&target, shell.as_deref()),
        Command::Fallback { rest } => cmd_fallback(&rest),
        Command::Feed { rest } => cmd_feed(&rest),
        Command::Proxy { rest } => cmd_proxy(&rest),
        Command::Doctor => doctor::run(),
        Command::Run { .. } => anyhow::bail!(
            "`clauth run` isn't a command; for a headless delegate use \
             `clauth start <profile> -p \"<prompt>\"` (or the MCP `delegate` tool)"
        ),
        // A bare word is a profile name. More than one word is nothing clauth
        // knows: a usage error rather than the old help-and-exit-0, so a typo
        // is distinguishable from success to a calling script.
        Command::External(words) => match words.as_slice() {
            [name] => cmd_switch(name),
            _ => Err(usage_error(format!(
                "unrecognized command '{}'; run `clauth --help` for the command list",
                words.join(" ")
            ))),
        },
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

/// `clauth fallback <op> …` — chain edits over the `fallback_config`
/// primitives (CDX-4 C1: membership ops route by the profile's harness into
/// the matching chain, so this one surface edits BOTH chains). The daemon
/// picks external `profiles.toml` edits up on its mtime watch, exactly like a
/// hand edit; nothing here needs the socket.
/// CLA-FEED: `clauth feed <profile> <on|off>` — feed the profile's session
/// token from its clauth-private usage chain (sessions get full-scope,
/// `subscriptionType`-stamped bearers → Fable-capable) or restore the static
/// long-lived mint. See `docs/cla-feed/DESIGN.md`.
fn cmd_feed(rest: &[String]) -> Result<()> {
    const USAGE: &str = "usage: clauth feed <profile> <on|off>";
    let [name, onoff] = rest else {
        anyhow::bail!("{USAGE}");
    };
    let on = match onoff.as_str() {
        "on" => true,
        "off" => false,
        _ => anyhow::bail!("{USAGE}"),
    };
    let mut config = load_config()?;
    let Some(canonical) = config.canonical_name(name) else {
        anyhow::bail!("unknown profile '{name}'");
    };
    let Some(profile) = config.find(&canonical) else {
        anyhow::bail!("unknown profile '{name}'");
    };
    if profile.is_codex() {
        anyhow::bail!("'{canonical}' is a codex profile — the session feed is claude-only");
    }

    if !on {
        // The whole disable (flag flip + mint restore) serializes on the
        // profile's rotation guard: without it, a concurrent rotation that
        // still sees feed=on can re-feed the sidecar AFTER the restore,
        // leaving feed=off + an hours-horizon live credential + no backup
        // (review round 1: the orphaned-backup sign-out).
        let _guard = runtime::RotationGuard::acquire(&canonical)
            .map_err(|_| anyhow::anyhow!("'{canonical}' rotation lock busy — retry in a moment"))?;
        if let Some(profile) = config.find_mut(&canonical) {
            profile.session_feed = false;
            profile::save_profile(profile)?;
        }
        let is_active = config.is_active(&canonical);
        if claude::restore_static_mint(&canonical)? {
            println!("clauth: feed off for '{canonical}' — static long-lived mint restored.");
            if is_active {
                claude::force_link_profile_credentials(&canonical)?;
                println!("clauth: reinstalled live (Keychain updated).");
            }
        } else {
            println!(
                "clauth: feed off for '{canonical}' — no static backup to restore; the last \
                 fed token serves until its expiry. Re-mint with `clauth login {canonical} \
                 --setup-token`."
            );
        }
        return Ok(());
    }

    let Some(oauth) = profile
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
    else {
        anyhow::bail!(
            "'{canonical}' has no usage OAuth chain to feed from — `clauth login {canonical}` \
             first"
        );
    };
    let fable_capable = oauth
        .scopes
        .as_ref()
        .is_some_and(|s| s.iter().any(|x| x == "user:profile"))
        && oauth.subscription_type.is_some();
    if !fable_capable {
        println!(
            "clauth: warning — '{canonical}''s chain is missing the user:profile scope or a \
             subscriptionType stamp; fed tokens may not unlock plan-gated models. A fresh \
             `clauth login {canonical}` browser sign-in fixes the mint."
        );
    }
    // A mis-filled sidecar is pre-cleared here, where overwriting is explicit
    // operator intent — the evidence still goes to quarantine first.
    if claude::quarantine_misfilled_sidecar(&canonical)? {
        println!(
            "clauth: '{canonical}' had a mis-filled sidecar (rotating pair) — quarantined \
             under ~/.clauth/quarantine/ before arming."
        );
    }
    if let Some(profile) = config.find_mut(&canonical) {
        profile.session_feed = true;
        profile::save_profile(profile)?;
    }
    let is_active = config.is_active(&canonical);
    let handle: profile::ConfigHandle =
        std::sync::Arc::new(crate::lockorder::RankedMutex::new(config));
    oauth::arm_session_feed(&handle, &canonical, oauth::refresh_result)?;
    println!("clauth: feed on for '{canonical}' — session token armed from the usage chain.");
    if is_active {
        claude::force_link_profile_credentials(&canonical)?;
        println!("clauth: installed live (Keychain updated) — new sessions run on the fed token.");
    } else {
        println!("clauth: it installs on the next switch:  clauth {canonical}");
    }
    Ok(())
}

fn cmd_fallback(rest: &[String]) -> Result<()> {
    const USAGE: &str = "usage: clauth fallback list | add <profile> | remove <profile> | \
                         up <profile> | down <profile> | threshold <profile> <pct> | \
                         last-resort <profile> <on|off>";
    let mut config = load_config()?;
    match rest {
        [sub] if sub == "list" => {
            let line = |names: &[crate::profile::ProfileName]| {
                if names.is_empty() {
                    "(empty)".to_string()
                } else {
                    names
                        .iter()
                        .map(|n| n.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                }
            };
            println!("claude chain: {}", line(&config.state.fallback_chain));
            println!("codex  chain: {}", line(&config.state.codex_fallback_chain));
            Ok(())
        }
        [sub, name] if sub == "add" => {
            if fallback_config::add(&mut config, name)? {
                println!("clauth: added '{name}' to its harness's fallback chain");
            } else {
                println!("clauth: '{name}' is already a chain member");
            }
            Ok(())
        }
        [sub, name] if sub == "remove" => {
            if fallback_config::remove(&mut config, name)? {
                println!("clauth: removed '{name}' from its chain");
            } else {
                println!("clauth: '{name}' was not a chain member");
            }
            Ok(())
        }
        [sub, name] if sub == "up" || sub == "down" => {
            let Some(dir) = fallback_config::MoveDir::parse(sub) else {
                anyhow::bail!("{USAGE}"); // unreachable: guarded by the match arm
            };
            if fallback_config::move_member(&mut config, name, dir)? {
                println!("clauth: moved '{name}' {sub}");
            } else {
                println!("clauth: '{name}' did not move (not a member, or at the boundary)");
            }
            Ok(())
        }
        [sub, name, pct] if sub == "threshold" => {
            let value: f64 = pct
                .parse()
                .map_err(|_| anyhow::anyhow!("threshold must be a number, got '{pct}'"))?;
            fallback_config::set_threshold(&mut config, name, value)?;
            println!("clauth: '{name}' switch threshold set to {value}%");
            Ok(())
        }
        [sub, name, onoff] if sub == "last-resort" => {
            let on = match onoff.as_str() {
                "on" => true,
                "off" => false,
                _ => anyhow::bail!("{USAGE}"),
            };
            fallback_config::set_last_resort(&mut config, name, on)?;
            println!(
                "clauth: '{name}' last-resort mark {}",
                if on { "set" } else { "cleared" }
            );
            Ok(())
        }
        _ => anyhow::bail!("{USAGE}"),
    }
}

/// `clauth proxy [--port N]` — run the CDX-5 localhost injection proxy;
/// `clauth proxy --print-config [--port N]` — print the `config.toml` block
/// to paste (clauth never edits the live config). Opt-in; live upstream is
/// AX-manual acceptance.
fn cmd_proxy(rest: &[String]) -> Result<()> {
    let mut port = proxy::DEFAULT_PROXY_PORT;
    let mut print_only = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--print-config" => {
                print_only = true;
                i += 1;
            }
            "--port" => {
                let value = rest
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?;
                port = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid port: {value}"))?;
                i += 2;
            }
            other => anyhow::bail!(
                "usage: clauth proxy [--port N] | clauth proxy --print-config [--port N] \
                 (got '{other}')"
            ),
        }
    }
    if print_only {
        proxy::print_config(port);
        Ok(())
    } else {
        platform::init();
        proxy::run(port)
    }
}

fn cmd_start(
    name: &str,
    rest: &[String],
    isolation: Isolation,
    rescue_override: Option<bool>,
) -> Result<()> {
    platform::init();
    runtime::gc_stale_runtimes();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    refuse_if_disabled(&config, &canonical)?;
    // CDX-1b: a codex profile starts `codex` against its isolated CODEX_HOME.
    if config.find(&canonical).is_some_and(|p| p.is_codex()) {
        if isolation == Isolation::Isolated {
            anyhow::bail!(
                "codex starts are always isolated (their own CODEX_HOME) — drop --isolated"
            );
        }
        return start::run_codex(&canonical, rest);
    }
    start::run(&config, &canonical, rest, isolation, None, rescue_override)
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
    use std::io::{IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(true);
    }
    let object = if is_api {
        "endpoint + API key"
    } else {
        "stored credentials"
    };
    print!(
        "clauth: profile '{target}' already exists. Re-authenticating replaces its {object}. Continue? [y/N] "
    );
    std::io::stdout().flush()?;
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
    use std::io::{IsTerminal as _, Write as _};
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
            print!("Base URL: ");
            std::io::stdout().flush()?;
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
            eprintln!(
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
/// tokens, login summary, identity-anchor probe) and wrap it in a capture
/// snapshot. Shared by `cmd_login`'s new and reauth OAuth arms so the two stay
/// in lockstep. Takes `config` for the CAP-3 sibling-ownership check below.
fn run_oauth_browser(
    config: &AppConfig,
    reauth: bool,
    target: &str,
) -> Result<actions::CaptureSnapshot> {
    if reauth {
        println!("clauth: re-authenticating existing profile '{target}', opening a browser…");
    } else {
        println!("clauth: opening a browser to log in to a new account for '{target}'…");
    }
    let outcome = oauth_login::login_with(|progress| {
        // The CLI surfaces only the paste-fallback URL; the later milestones
        // are TUI-modal fodder and would just be noise between the prints here.
        if let oauth_login::LoginProgress::AuthorizeUrl(url) = progress {
            println!("\nIf the browser didn't open, visit this URL to authorize:\n{url}\n");
        }
    })?;
    println!(
        "clauth: login complete.\n{}",
        oauth_login::login_summary(&outcome.credentials)
    );
    // Probe the identity anchor for unattended mirror adoption (best-effort —
    // a probe failure never fails the login): the account uuid+email this
    // login authenticates as, cached per profile so
    // `oauth::try_adopt_live_rotation` can verify a diverged live login is the
    // SAME account even after the stored token dies. The identity rides the
    // snapshot (CAP-2) so the capture writes it as the anchor — writing it
    // here first was the 2026-07-12 pollution: the capture re-anchored from
    // the UNRELATED live login's hint, clobbering this authoritative probe.
    let probed = outcome
        .credentials
        .claude_ai_oauth
        .as_ref()
        .map(|o| o.access_token.clone())
        .and_then(|tok| crate::usage::fetch_account_identity(&tok));
    // CAP-3: refuse to store an account a SIBLING profile already holds — two
    // profiles polling one account is a self-inflicted rate-limit pin
    // (2026-07-12, twice: a blind capture, then a wrong-account re-login).
    // Nothing has been written yet, so the refusal is side-effect-free; the
    // freshly minted tokens are simply discarded.
    if let Some(id) = &probed
        && let Some(owner) = actions::account_owner(config, id, target)
    {
        let who = id.email.as_deref().unwrap_or(&id.uuid);
        anyhow::bail!(
            "this browser login is {who}, and profile '{owner}' already holds that \
             account — storing it twice would make both profiles double-poll it \
             into a rate limit. Nothing was saved. To refresh that account, run \
             `clauth login {owner}`; to put a DIFFERENT account into '{target}', \
             sign claude.com out in the browser, log into that account, and rerun."
        );
    }
    let identity = probed.map_or(
        actions::CaptureIdentity::Unknown,
        actions::CaptureIdentity::Known,
    );
    Ok(actions::CaptureSnapshot {
        credentials: Some(outcome.credentials),
        base_url: None,
        api_key: None,
        identity,
    })
}

/// `clauth login <name> [--base-url <url>] [--api-key <key>] [--model <id>]
/// [--new]` — add a new account or re-authenticate an existing one in place
/// (#7). The auth method is flag-selected: bare (no `--base-url`/`--api-key`)
/// runs the browser OAuth flow (`oauth_login`) and writes the minted tokens
/// straight into the profile's `.credentials.json`, identically on every
/// platform; passing either endpoint flag switches to API-key mode and captures
/// a base_url + api_key pair instead, prompting (echo-off for the key) for
/// whatever a flag omitted.
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
/// `--new` refuses the reauth route entirely. Tokens are never printed — only
/// a sha256 prefix.
fn cmd_login(args: LoginArgs) -> Result<()> {
    platform::init();
    let mut config = load_config()?;
    let route = login_route(&config, &args.profile);
    let target = match &route {
        LoginRoute::Reauth(existing) => existing.clone(),
        LoginRoute::New(fresh) => {
            actions::validate_profile_name(fresh, &config.names(), None)?;
            fresh.clone()
        }
    };
    let reauth = matches!(route, LoginRoute::Reauth(_));
    let is_api = args.is_api_mode();

    // CDX-1: harness immutability, reverse direction. Without this guard a
    // bare `clauth login <codex-profile>` falls into the claude reauth path,
    // writing claude credentials into a codex profile — a corrupt hybrid that
    // also re-enters the Anthropic fetch legs (breaking the §0.1 exclusion
    // invariant). The forward direction (`--codex` at a claude profile) is
    // guarded inside `codex_capture_into_profile`; the writer-level backstop
    // lives in `overwrite_captured_profile`. Ahead of the `--setup-token`
    // branch so a claude-shaped sidecar can never land in a codex profile
    // either.
    if reauth && !args.codex && config.find(&target).is_some_and(|p| p.is_codex()) {
        anyhow::bail!(
            "profile '{target}' is a codex profile — re-auth it with: clauth login {target} --codex"
        );
    }

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

    // `--new` pins the CREATE semantics: refuse to touch an existing profile
    // instead of routing to the reauth overwrite. This is the race-proof
    // collision guard for non-TTY callers (a menu-bar app spawning
    // `clauth login` gets no confirm prompt, and any UI-side pre-check is a
    // TOCTOU against this process's freshly-loaded config) — the refusal
    // happens HERE, against current state, so a name minted out-of-band a
    // moment ago can never be silently re-authenticated.
    if args.new_only && reauth {
        anyhow::bail!(
            "profile '{target}' already exists and --new forbids re-authenticating it. \
             Rerun without --new to refresh '{target}', or pick a different name."
        );
    }

    // Confirm a reauth BEFORE collecting anything (browser or key prompt): a
    // declined overwrite must not open a browser or read a secret.
    if reauth && !confirm_reauth(&target, is_api)? {
        println!("clauth: aborted. '{target}' left unchanged.");
        return Ok(());
    }

    // CDX-3 R5: `--codex --browser` mints a fresh codex login via the PKCE
    // loopback flow straight into the profile store — the live
    // ~/.codex/auth.json and the codex active slot are never touched.
    if args.codex && args.browser {
        let bytes = codex::login::browser_login_snapshot(|p| match p {
            codex::login::CodexLoginProgress::AuthorizeUrl(url) => {
                println!("clauth: opening the browser for a codex login…");
                println!("  if it doesn't open, paste this URL yourself:\n  {url}");
            }
            codex::login::CodexLoginProgress::ExchangingCode => {
                println!("clauth: login callback received — exchanging the code…");
            }
        })?;
        actions::codex_store_browser_login(&mut config, &target, &bytes)?;
        println!(
            "clauth: stored the new codex login in profile '{target}' (the live codex \
             login is unchanged). Switch to it with:  clauth {target}"
        );
        return Ok(());
    }

    // CDX-1 T5: `--codex` captures the live ~/.codex/auth.json — no browser,
    // no prompt collection. The actions layer owns every guard (store mode,
    // live presence, account dedup, harness immutability).
    if args.codex {
        actions::codex_capture_into_profile(&mut config, &target)?;
        println!(
            "clauth: captured the live codex login into profile '{target}'. \
             Switch codex accounts with:  clauth {target}"
        );
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
                // No OAuth identity to anchor: an API-key endpoint carries no
                // claude.ai account. Unknown drops a stale anchor left by a
                // previous OAuth incarnation of this profile (CAP-2).
                identity: actions::CaptureIdentity::Unknown,
            }
        } else {
            run_oauth_browser(&config, true, &target)?
        };
        actions::overwrite_captured_profile(&mut config, &target, snapshot)?;
        // On a reauth `--model` is an explicit override; without it the
        // profile's existing model settings survive.
        if let Some(model) = args.model.as_deref() {
            actions::set_profile_default_model(&mut config, &target, model)?;
        }
        let what = if is_api { "endpoint + key" } else { "tokens" };
        println!("clauth: re-authenticated '{target}'. Fresh {what} are in place.");
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
            target.clone(),
            base_url,
            api_key,
            args.model.clone(),
        )?;
        println!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    } else {
        let snapshot = run_oauth_browser(&config, false, &target)?;
        actions::capture_into_profile(&mut config, target.clone(), snapshot)?;
        // Apply the requested default model so the captured profile's sessions
        // route there from the first launch.
        if let Some(model) = args.model.as_deref() {
            actions::set_profile_default_model(&mut config, &target, model)?;
        }
        println!("clauth: captured into profile '{target}'. Switch to it with:  clauth {target}");
    }
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
fn cmd_login_setup_token(
    config: &mut profile::AppConfig,
    target: &str,
    exists: bool,
    model: Option<&str>,
    yes: bool,
) -> Result<()> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal();

    // Replacing an existing sidecar re-points every future switch at the new
    // token — confirm like the other in-place replacements. A fresh capture
    // (no sidecar yet) is additive and needs no ceremony.
    if claude::session_token_status(target).is_some() && !yes {
        if !interactive {
            anyhow::bail!(
                "'{target}' already has a long-lived token; pass --yes to replace it non-interactively"
            );
        }
        print!("Replace the stored long-lived token for '{target}'? [y/N] ");
        std::io::Write::flush(&mut std::io::stdout())?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. '{target}' left unchanged.");
            return Ok(());
        }
    }

    let raw = if interactive {
        println!("clauth: capturing a long-lived token for '{target}'.");
        println!("  1. in another terminal, run:  claude setup-token");
        println!("  2. complete the browser flow it opens");
        println!("  3. paste the minted token below (input stays hidden)");
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
        actions::set_profile_default_model(config, target, model)?;
    }

    // CLA-FEED: on a feed-enabled profile the next rotation overwrites this
    // mint with a fed value — capture the mint into the sidecar AND the
    // degrade backup atomically (one flock section, same bytes; a two-step
    // write-then-copy can snapshot a concurrent rotation's fed token as "the
    // mint").
    let feed_on = config.find(target).is_some_and(|p| p.session_feed);
    let now = crate::usage::now_ms() as i64;
    let expires_at = if feed_on {
        claude::write_session_token_with_backup(target, &token, now)?
    } else {
        claude::write_session_token(target, &token, now)?
    };
    let days = (expires_at - crate::usage::now_ms() as i64) / 86_400_000;
    println!(
        "clauth: long-lived token installed for '{target}' · assumed to expire in ~{days}d \
         (`claude setup-token` mints last about a year)."
    );
    if feed_on {
        println!(
            "clauth: the session feed is on for '{target}' — this mint is preserved as the \
             degrade fallback; rotations keep feeding the live token."
        );
    }
    println!(
        "clauth: it takes effect on the next switch:  clauth {target}{}",
        if exists {
            ""
        } else {
            "\nclauth: for usage polling, also add an OAuth pair later:  clauth login <name>"
        }
    );
    Ok(())
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
        use std::io::{IsTerminal as _, Write as _};
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "refusing to delete '{canonical}' without confirmation; pass --yes for a non-interactive delete"
            );
        }
        print!("clauth: delete profile '{canonical}' and all its credentials? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. '{canonical}' left in place.");
            return Ok(());
        }
    }
    let was_active = config.is_active(&canonical);
    actions::delete_profile(&mut config, &canonical, force)?;
    if was_active {
        println!("clauth: deleted profile '{canonical}' (was active; live credentials cleared).");
    } else {
        println!("clauth: deleted profile '{canonical}'.");
    }
    Ok(())
}

/// Refuse `name` as a switch/start target when it's user-disabled, naming the
/// fix. Called by [`cmd_switch`] and [`cmd_start`] as a friendly early check,
/// and by `start::run` — the authoritative chokepoint every session-spawn path
/// (`cmd_start`, `sessions_cli::run_resume`) funnels through — so all three
/// callers share one message instead of drifting.
fn refuse_if_disabled(config: &AppConfig, name: &str) -> Result<()> {
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
        println!("clauth: '{canonical}' is already disabled.");
        return Ok(());
    }

    if !yes {
        use std::io::{IsTerminal as _, Write as _};
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            anyhow::bail!(
                "refusing to disable '{canonical}' without confirmation; pass --yes for a non-interactive run"
            );
        }
        print!(
            "clauth: disable profile '{canonical}'? it drops out of auto-switch and usage \
             polling until re-enabled. [y/N] "
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. '{canonical}' left unchanged.");
            return Ok(());
        }
    }

    actions::disable_profile(&mut config, &canonical)?;
    println!("clauth: disabled '{canonical}'.");
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
        println!("clauth: enabled '{canonical}'.");
    } else {
        println!("clauth: '{canonical}' is already enabled.");
    }
    Ok(())
}

fn cmd_switch(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    refuse_if_disabled(&config, &canonical)?;
    // CDX-1 T5: `clauth <name>` stays THE switch verb — the target's harness
    // picks the path. The claude path is untouched.
    if config.find(&canonical).is_some_and(|p| p.is_codex()) {
        return cmd_switch_codex(config, &canonical);
    }
    actions::switch_profile_cli(config, &canonical)
}

/// CLI codex switch: loss-free by default; a FOREIGN live login (matching no
/// stored codex profile) asks `[y/N]` whether to archive it to quarantine and
/// continue. Non-TTY refuses with the actionable message instead — a script
/// must not silently displace a login clauth doesn't own.
fn cmd_switch_codex(mut config: AppConfig, canonical: &str) -> Result<()> {
    use std::io::{IsTerminal as _, Write as _};

    let mut policy = actions::ForeignLivePolicy::Refuse;
    if actions::codex_live_is_foreign(&config)?
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        print!(
            "clauth: the live codex login matches no stored profile. Archive it to \
             ~/.clauth/quarantine and switch anyway? [y/N] "
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !reauth_confirmed(&answer) {
            println!("clauth: aborted. Live codex login left in place.");
            return Ok(());
        }
        policy = actions::ForeignLivePolicy::Archive;
    }

    let report = actions::codex_switch_profile(&mut config, canonical, policy)?;
    if let Some(owner) = &report.adopted_back {
        println!("clauth: adopted the refreshed live login back into '{owner}' first.");
    }
    if let Some(path) = &report.archived {
        println!(
            "clauth: archived the outgoing live login to {}.",
            path.display()
        );
    }
    println!("clauth: codex now uses '{canonical}'.");
    if codex::codex_processes_running() {
        println!(
            "clauth: note — running codex sessions keep their current account until they \
             exit; the switch applies to new sessions."
        );
    }
    Ok(())
}

/// CDX-1c semi-seamless carryover: switch the live codex login to `name`,
/// then run `codex resume --last` in this terminal — the most recent
/// conversation continues under the new account (codex re-reads auth.json
/// fresh at startup, and `store=false` means the resumed context is resent
/// whole, so the serving account can change mid-conversation at a session
/// boundary). The claude side needs no analogue: its live hot-swap already
/// carries running sessions across.
fn cmd_resume(name: &str) -> Result<()> {
    platform::init();
    let config = load_config()?;
    let canonical = resolve_or_bail(&config, name)?;
    if !config.find(&canonical).is_some_and(|p| p.is_codex()) {
        anyhow::bail!(
            "'{canonical}' is a claude profile — `clauth resume <profile>` is the codex \
             carryover (a claude switch hot-swaps running sessions already; to resume a \
             claude SESSION, pass its id or `latest`)"
        );
    }
    cmd_switch_codex(config, &canonical)?;
    let status = std::process::Command::new("codex")
        .args(["resume", "--last"])
        .status()
        .context("failed to launch `codex resume --last` (is codex on PATH?)")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Routes the bare two-token `clauth resume <target>`: a target naming a KNOWN
/// codex profile takes the codex carryover; everything else (session id,
/// `latest`) is the claude session resume. Config-lookup–based, so a claude
/// profile name still reaches `cmd_resume`'s explanatory bail rather than a
/// confusing "no such session".
fn cmd_resume_dispatch(target: &str) -> Result<()> {
    let is_profile = load_config()
        .ok()
        .is_some_and(|c| c.canonical_name(target).is_some());
    if is_profile {
        cmd_resume(target)
    } else {
        sessions_cli::run_resume(target, None)
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
    // `load_profile` is permissive — a missing `config.toml` reads as the
    // default profile, so a helper pointing at a typo'd or deleted name would
    // otherwise return `Ok(None)` indistinguishable from a real no-key
    // profile. The dir-existence check fails closed with a clearer message
    // instead; both cases still surface as exit 1 via `cmd_api_key`.
    if !profile::profile_dir(name)?.exists() {
        anyhow::bail!("profile '{name}' not found");
    }
    let profile = profile::load_profile(name)?;
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
    tui::run(config)
}

/// Feature→test traceability map.
#[cfg(test)]
#[path = "../tests/inline/feature_coverage.rs"]
mod feature_coverage;

#[cfg(test)]
#[path = "../tests/inline/cli.rs"]
mod tests;
