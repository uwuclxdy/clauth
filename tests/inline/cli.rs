//! The CLI grammar, driven through the derived clap parser rather than any
//! hand-rolled shape check — a grammar edit in `src/cli.rs` reds these. Arms
//! whose success path spawns a browser, a `claude` process, or a scheduler are
//! asserted at the parse layer only; the side-effecting handlers keep their own
//! coverage (`tests/inline/actions.rs` for model persistence, the
//! `disabled_target_refusal` module below for the refusal chokepoints).

use super::*;

use clap::CommandFactory as _;

use crate::cli::StartArgs;

/// Parse an argv WITHOUT the binary name, the way `main` does (it passes
/// `args_os()`, whose first element is the binary).
fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("clauth").chain(args.iter().copied()))
}

/// Parse and unwrap to the subcommand, for the arms that must parse.
fn command(args: &[&str]) -> Command {
    parse(args)
        .unwrap_or_else(|e| panic!("{args:?} must parse, got: {e}"))
        .command
        .unwrap_or_else(|| panic!("{args:?} must select a subcommand"))
}

/// clap's half of the exit contract: the parse-error code on a malformed argv,
/// 0 on a successful parse. Only the first of `main`'s two stages; it never
/// runs `crate::dispatch`, so an argv that parses but fails in dispatch reads 0
/// here. For the full parse -> dispatch -> exit_code mapping see
/// `dispatch_exit_code`.
fn parse_exit_code(args: &[&str]) -> i32 {
    parse(args).err().map(|e| e.exit_code()).unwrap_or(0)
}

// ── the three shapes that are not plain subcommands ─────────────────────────

/// A bare `clauth` selects no subcommand, which is what routes `dispatch` to
/// the TUI.
#[test]
fn bare_invocation_selects_no_subcommand() {
    let cli = parse(&[]).expect("bare clauth must parse");
    assert!(
        cli.command.is_none(),
        "no subcommand is what sends dispatch to the TUI"
    );
    assert_eq!(cli.theme, None);
}

/// A bare unrecognized word is a profile name, captured as the external
/// subcommand so `dispatch` can switch to it.
#[test]
fn bare_word_is_captured_as_a_profile_name() {
    match command(&["acme"]) {
        Command::External(words) => assert_eq!(words, ["acme"]),
        other => panic!("a bare word must reach the external arm, got {other:?}"),
    }
}

/// A real subcommand shadows a same-named profile in the bare-word position —
/// the precedence the hand-rolled dispatcher had, kept deliberately.
#[test]
fn a_subcommand_name_shadows_a_same_named_profile() {
    assert!(
        matches!(command(&["which"]), Command::Which { .. }),
        "`clauth which` must stay the subcommand even if a profile is named `which`"
    );
    assert!(
        matches!(command(&["mcp"]), Command::Mcp),
        "`clauth mcp` must stay the subcommand"
    );
    // clap generates a `help` subcommand, so `clauth help` prints the command
    // table (exit 0) where it used to try switching to a profile named `help`.
    // That follows the same precedence rather than breaking it, and `clauth
    // help <cmd>` is worth the one name; pinned so it stays a decision.
    let err = parse(&["help"]).expect_err("clap reports its help subcommand as an Err");
    assert_eq!(err.exit_code(), 0);
}

/// `start` hands `claude` everything after the profile byte-identically,
/// leading hyphens included, so a passthrough `-p`/`--model` is never eaten as
/// a clauth flag.
#[test]
fn start_forwards_claude_args_verbatim_including_leading_hyphens() {
    let Command::Start(a) = command(&["start", "acme", "-p", "hi", "--model", "opus"]) else {
        panic!("start must parse");
    };
    assert_eq!(a.profile, "acme");
    assert_eq!(a.claude_args, ["-p", "hi", "--model", "opus"]);
    assert_eq!(a.isolation(), Isolation::Shared);
    assert_eq!(a.rescue_override(), None);
}

/// Where clauth's half of the grammar actually ends, pinned because it MOVED in
/// the clap port and the difference is silent. The hand-rolled parser stopped at
/// the profile name and forwarded every later token; clap keeps recognizing
/// `start`'s own flags past it, and only hands over on a token `start` does not
/// declare. `claude` has no `--isolated`/`--rescue`/`--no-rescue`, so the only
/// spelling this reaches in practice is `--help`, and `--` forwards even that.
#[test]
fn clauths_own_start_flags_are_still_recognized_after_the_profile_name() {
    let Command::Start(a) = command(&["start", "acme", "--isolated", "--rescue"]) else {
        panic!("start must parse");
    };
    assert!(
        a.isolated,
        "clap keeps parsing start's own flags past the name"
    );
    assert_eq!(a.rescue_override(), Some(true));
    assert!(a.claude_args.is_empty());

    // A token `start` does not declare hands over, and everything behind it
    // follows verbatim even when it collides later.
    let Command::Start(b) = command(&["start", "acme", "-p", "hi", "--isolated"]) else {
        panic!("start must parse");
    };
    assert!(
        !b.isolated,
        "once the passthrough starts, a clauth spelling is claude's"
    );
    assert_eq!(b.claude_args, ["-p", "hi", "--isolated"]);

    // `--` is the escape for the collision, and the shape the README documents.
    let Command::Start(c) = command(&["start", "acme", "--", "--isolated"]) else {
        panic!("start must parse");
    };
    assert!(!c.isolated);
    assert_eq!(c.claude_args, ["--isolated"]);
}

/// The README documents `clauth start <profile> -- <claude args>`. clap eats a
/// first bare `--` as its end-of-flags marker, so it does NOT reach `claude`;
/// every arg after it does, unchanged. Pinned because the separator is a
/// documented spelling and its handling is silent either way.
#[test]
fn start_consumes_a_leading_double_dash_separator_and_forwards_the_rest() {
    let Command::Start(a) = command(&["start", "acme", "--", "--model", "haiku"]) else {
        panic!("start must parse");
    };
    assert_eq!(a.profile, "acme");
    assert_eq!(
        a.claude_args,
        ["--model", "haiku"],
        "the separator is clap's, the args behind it are claude's"
    );
}

// ── start's own flags ───────────────────────────────────────────────────────

#[test]
fn start_isolated_flag_precedes_the_name() {
    let Command::Start(a) = command(&["start", "--isolated", "acme", "-p", "hi"]) else {
        panic!("start must parse");
    };
    assert_eq!(a.profile, "acme");
    assert_eq!(a.isolation(), Isolation::Isolated);
    assert_eq!(a.rescue_override(), None);
    assert_eq!(a.claude_args, ["-p", "hi"]);
}

#[test]
fn start_rescue_flags_override_in_any_order() {
    let Command::Start(on) = command(&["start", "--rescue", "--isolated", "acme"]) else {
        panic!("must parse");
    };
    assert_eq!(on.rescue_override(), Some(true));
    assert_eq!(on.isolation(), Isolation::Isolated);

    let Command::Start(off) = command(&["start", "--isolated", "--no-rescue", "acme"]) else {
        panic!("must parse");
    };
    assert_eq!(off.rescue_override(), Some(false));
}

/// Both rescue spellings on one line is last-one-wins, not a rejection — the
/// hand-rolled parser's behavior, preserved by clap's mutual `overrides_with`.
#[test]
fn start_last_rescue_spelling_wins() {
    let Command::Start(a) = command(&["start", "--isolated", "--rescue", "--no-rescue", "acme"])
    else {
        panic!("must parse");
    };
    assert_eq!(a.rescue_override(), Some(false));

    let Command::Start(b) = command(&["start", "--isolated", "--no-rescue", "--rescue", "acme"])
    else {
        panic!("must parse");
    };
    assert_eq!(b.rescue_override(), Some(true));
}

/// Rescue lifts a throwaway isolated store into the global one; a shared start
/// already writes there, so the flags without `--isolated` are a user error,
/// rejected rather than silently no-op'd.
#[test]
fn start_rescue_requires_isolated() {
    for args in [
        ["start", "--rescue", "acme"].as_slice(),
        ["start", "--no-rescue", "acme"].as_slice(),
    ] {
        let err = parse(args).expect_err("a rescue flag without --isolated must be refused");
        assert_eq!(err.exit_code(), 2, "a bad flag combination exits 2");
        assert!(
            err.to_string().contains("--isolated"),
            "the error must name the missing flag, got: {err}"
        );
    }
}

/// Missing-required-argument failures like this one exited 1 before the port
/// (a plain `anyhow::bail!` carrying a `usage:` string) while only the
/// sessions/resume/info surface used the `UsageError` 2. clap normalizes the
/// whole grammar onto 2, so a caller no longer has to know which command it
/// typed wrong to read the code.
#[test]
fn start_requires_a_profile_name() {
    for args in [
        ["start"].as_slice(),
        ["start", "--isolated"].as_slice(),
        ["start", "--isolated", "--rescue"].as_slice(),
    ] {
        assert_eq!(
            parse_exit_code(args),
            2,
            "{args:?}: flags without a name must be a usage error"
        );
    }
}

/// `--isolated` alone still needs no rescue decision, and the accessor pair is
/// what `dispatch` hands `start::run`.
#[test]
fn start_args_accessors_map_flags_to_the_runtime_types() {
    let shared = StartArgs {
        isolated: false,
        rescue: false,
        no_rescue: false,
        profile: "acme".into(),
        claude_args: Vec::new(),
    };
    assert_eq!(shared.isolation(), Isolation::Shared);
    assert_eq!(shared.rescue_override(), None);

    let isolated = StartArgs {
        isolated: true,
        rescue: true,
        no_rescue: false,
        profile: "acme".into(),
        claude_args: Vec::new(),
    };
    assert_eq!(isolated.isolation(), Isolation::Isolated);
    assert_eq!(isolated.rescue_override(), Some(true));
}

// ── login ───────────────────────────────────────────────────────────────────

fn login(args: &[&str]) -> LoginArgs {
    match command(args) {
        Command::Login(a) => a,
        other => panic!("{args:?} must reach login, got {other:?}"),
    }
}

#[test]
fn login_bare_name_is_oauth_mode() {
    let a = login(&["login", "acme"]);
    assert_eq!(a.profile, "acme");
    assert_eq!(a.model, None);
    assert!(!a.is_api_mode());
    assert!(!a.setup_token);
    assert!(!a.yes);
}

#[test]
fn login_accepts_a_short_alias_or_a_full_custom_model_id() {
    assert_eq!(
        login(&["login", "acme", "--model", "opus"])
            .model
            .as_deref(),
        Some("opus")
    );
    assert_eq!(
        login(&["login", "acme", "--model", "claude-opus-4-8"])
            .model
            .as_deref(),
        Some("claude-opus-4-8")
    );
}

#[test]
fn login_setup_token_flag_and_its_unprompted_replace() {
    let a = login(&["login", "acme", "--setup-token"]);
    assert!(a.setup_token);
    assert!(!a.yes);
    assert!(login(&["login", "acme", "--setup-token", "--yes"]).yes);
    assert!(
        login(&["login", "acme", "--setup-token", "-y"]).yes,
        "-y is the short spelling"
    );
}

/// The sidecar capture and the API-key pair are different logins — the
/// combination is a contradiction, not a preference. `--yes` means nothing
/// outside the capture flow.
#[test]
fn login_setup_token_excludes_api_mode_and_bare_yes() {
    let err = parse(&["login", "acme", "--setup-token", "--base-url", "https://x"])
        .expect_err("setup-token + api mode must be refused");
    assert!(
        err.to_string().contains("cannot be used with"),
        "must read as a conflict, got: {err}"
    );
    assert_eq!(err.exit_code(), 2);

    let err = parse(&["login", "acme", "--yes"]).expect_err("bare --yes must be refused");
    assert!(
        err.to_string().contains("--setup-token"),
        "the error must name what --yes requires, got: {err}"
    );
}

#[test]
fn login_api_mode_takes_both_endpoint_flags_in_any_order_with_model() {
    let a = login(&[
        "login",
        "deepseek",
        "--api-key",
        "sk-x",
        "--model",
        "deepseek-chat",
        "--base-url",
        "https://api.deepseek.com",
    ]);
    assert_eq!(a.profile, "deepseek");
    assert_eq!(a.base_url.as_deref(), Some("https://api.deepseek.com"));
    assert_eq!(a.api_key.as_deref(), Some("sk-x"));
    assert_eq!(a.model.as_deref(), Some("deepseek-chat"));
    assert!(a.is_api_mode());
}

/// Only one endpoint flag still selects API-key mode; the other is prompted at
/// runtime by `collect_api_endpoint`.
#[test]
fn login_api_mode_one_flag_leaves_the_other_for_the_prompt() {
    let a = login(&["login", "acme", "--api-key", "sk-x"]);
    assert_eq!(a.base_url, None);
    assert_eq!(a.api_key.as_deref(), Some("sk-x"));
    assert!(a.is_api_mode());
}

/// A value flag with nothing after it, and one whose "value" is the next flag
/// (`--base-url --api-key` is a forgotten base-url value), are both refused
/// rather than swallowing the following token.
#[test]
fn login_value_flags_reject_a_missing_or_flag_shaped_value() {
    for args in [
        ["login", "acme", "--model"].as_slice(),
        ["login", "acme", "--base-url"].as_slice(),
        ["login", "acme", "--api-key"].as_slice(),
        ["login", "acme", "--base-url", "--api-key", "sk-x"].as_slice(),
    ] {
        assert_eq!(
            parse_exit_code(args),
            2,
            "{args:?}: a missing or flag-shaped value must be a usage error"
        );
    }
}

/// `clauth login --model` (value forgotten, name missing) must be refused
/// instead of creating a profile literally named `--model`.
#[test]
fn login_rejects_flag_shaped_profile_names_and_a_second_positional() {
    for args in [
        ["login"].as_slice(),
        ["login", "--model"].as_slice(),
        ["login", "--model", "--model", "opus"].as_slice(),
        ["login", "acme", "--bogus", "x"].as_slice(),
        ["login", "acme", "--model", "opus", "extra"].as_slice(),
    ] {
        assert_eq!(parse_exit_code(args), 2, "{args:?} must be a usage error");
    }
}

// ── delete / disable / enable ───────────────────────────────────────────────

#[test]
fn delete_takes_yes_and_force_independently_in_any_order() {
    let Command::Delete {
        profile,
        yes,
        force,
    } = command(&["delete", "acme"])
    else {
        panic!("must parse");
    };
    assert_eq!((profile.as_str(), yes, force), ("acme", false, false));

    // --force overrides the live-session guard but does NOT skip the confirm.
    let Command::Delete { yes, force, .. } = command(&["delete", "acme", "--force"]) else {
        panic!("must parse");
    };
    assert_eq!(
        (yes, force),
        (false, true),
        "--force alone leaves yes unset"
    );

    let Command::Delete {
        profile,
        yes,
        force,
    } = command(&["delete", "--force", "-y", "acme"])
    else {
        panic!("must parse");
    };
    assert_eq!((profile.as_str(), yes, force), ("acme", true, true));
}

#[test]
fn delete_requires_a_name_and_rejects_an_unknown_flag_or_second_name() {
    for args in [
        ["delete"].as_slice(),
        ["delete", "--yes"].as_slice(),
        ["delete", "acme", "--bogus"].as_slice(),
        ["delete", "acme", "other"].as_slice(),
    ] {
        assert_eq!(parse_exit_code(args), 2, "{args:?} must be a usage error");
    }
}

#[test]
fn disable_takes_yes_but_has_no_force_override() {
    let Command::Disable { profile, yes } = command(&["disable", "-y", "acme"]) else {
        panic!("must parse");
    };
    assert_eq!((profile.as_str(), yes), ("acme", true));

    assert_eq!(
        parse_exit_code(&["disable", "acme", "--force"]),
        2,
        "disable has no --force override, unlike delete"
    );
    assert_eq!(parse_exit_code(&["disable"]), 2);
    assert_eq!(parse_exit_code(&["disable", "acme", "other"]), 2);
}

#[test]
fn enable_takes_exactly_one_name() {
    let Command::Enable { profile } = command(&["enable", "acme"]) else {
        panic!("must parse");
    };
    assert_eq!(profile, "acme");
    assert_eq!(parse_exit_code(&["enable"]), 2);
    assert_eq!(parse_exit_code(&["enable", "acme", "other"]), 2);
}

// ── which / sessions / resume / info ────────────────────────────────────────

#[test]
fn which_and_sessions_take_only_json() {
    assert!(matches!(
        command(&["which"]),
        Command::Which { json: false }
    ));
    assert!(matches!(
        command(&["which", "--json"]),
        Command::Which { json: true }
    ));
    assert!(matches!(
        command(&["sessions", "--json"]),
        Command::Sessions { json: true }
    ));
    assert_eq!(parse_exit_code(&["which", "extra"]), 2);
    assert_eq!(parse_exit_code(&["sessions", "extra"]), 2);
}

#[test]
fn resume_and_info_take_a_target_with_resume_alone_taking_a_profile() {
    let Command::Resume { target, profile } = command(&["resume", "latest"]) else {
        panic!("must parse");
    };
    assert_eq!((target.as_str(), profile), ("latest", None));

    let Command::Resume { target, profile } = command(&["resume", "abc123", "--profile", "acme"])
    else {
        panic!("must parse");
    };
    assert_eq!(target, "abc123");
    assert_eq!(profile.as_deref(), Some("acme"));

    let Command::Info { target } = command(&["info", "latest"]) else {
        panic!("must parse");
    };
    assert_eq!(target, "latest");

    assert_eq!(parse_exit_code(&["resume"]), 2);
    assert_eq!(parse_exit_code(&["info"]), 2);
    assert_eq!(
        parse_exit_code(&["info", "latest", "--profile", "acme"]),
        2,
        "info never launches, so it takes no profile"
    );
}

// ── daemon / status ─────────────────────────────────────────────────────────

#[test]
fn daemon_modes_are_mutually_exclusive_and_default_to_exit_if_running() {
    let Command::Daemon {
        standby,
        no_standby,
        replace,
        status,
    } = command(&["daemon"])
    else {
        panic!("must parse");
    };
    assert_eq!(
        (standby, no_standby, replace, status),
        (false, false, false, false),
        "bare `clauth daemon` picks no mode, which dispatch reads as exit-if-running"
    );

    for (args, flag) in [
        (["daemon", "--standby"].as_slice(), "standby"),
        (["daemon", "--no-standby"].as_slice(), "no_standby"),
        (["daemon", "--replace"].as_slice(), "replace"),
        (["daemon", "--status"].as_slice(), "status"),
    ] {
        let Command::Daemon {
            standby,
            no_standby,
            replace,
            status,
        } = command(args)
        else {
            panic!("{args:?} must parse");
        };
        let set = [
            ("standby", standby),
            ("no_standby", no_standby),
            ("replace", replace),
            ("status", status),
        ];
        for (name, value) in set {
            assert_eq!(
                value,
                name == flag,
                "{args:?}: {name} should be {}",
                name == flag
            );
        }
    }

    // Every pair conflicts, so no invocation can ask for two start modes.
    for pair in [
        ["--standby", "--no-standby"],
        ["--standby", "--replace"],
        ["--standby", "--status"],
        ["--no-standby", "--replace"],
        ["--no-standby", "--status"],
        ["--replace", "--status"],
    ] {
        assert_eq!(
            parse_exit_code(&["daemon", pair[0], pair[1]]),
            2,
            "daemon {pair:?} must be refused as a conflict"
        );
    }
    assert_eq!(parse_exit_code(&["daemon", "--nope"]), 2);
}

#[test]
fn status_requires_json_and_treats_disabled_as_an_alias_for_all() {
    let Command::Status {
        json,
        all,
        disabled,
    } = command(&["status", "--json"])
    else {
        panic!("must parse");
    };
    assert!(json);
    assert!(!all && !disabled);

    let Command::Status { all, disabled, .. } = command(&["status", "--json", "--disabled"]) else {
        panic!("must parse");
    };
    assert!(
        !all && disabled,
        "--disabled is its own flag, ORed with --all"
    );

    assert_eq!(
        parse_exit_code(&["status"]),
        2,
        "status has no output mode other than --json"
    );
    assert_eq!(parse_exit_code(&["status", "--all"]), 2);
    assert_eq!(parse_exit_code(&["status", "--json", "--bogus"]), 2);
}

// ── theme ───────────────────────────────────────────────────────────────────

/// Both spellings work, and the flag applies ahead of a subcommand the way the
/// peel-based predecessor did.
#[test]
fn theme_accepts_both_spellings_ahead_of_a_subcommand() {
    assert_eq!(
        parse(&["--theme=full"]).expect("= spelling").theme,
        Some(ThemeArg::Full)
    );
    assert_eq!(
        parse(&["--theme", "compatible"])
            .expect("space spelling")
            .theme,
        Some(ThemeArg::Compatible),
        "the space-separated spelling is new and must work"
    );
    let cli = parse(&["--theme=compatible", "which", "--json"]).expect("ahead of a subcommand");
    assert_eq!(cli.theme, Some(ThemeArg::Compatible));
    assert!(matches!(cli.command, Some(Command::Which { json: true })));

    assert_eq!(
        parse_exit_code(&["--theme", "bogus"]),
        2,
        "an unknown tier is a usage error, not a profile named --theme=bogus"
    );
}

// ── the hidden entry points ─────────────────────────────────────────────────

/// The three internal entry points must still dispatch when invoked directly
/// (CC's `apiKeyHelper`, the bundled `asyncRewake` hook, and the completion
/// scripts' name shellout all run them by name) while staying out of every
/// help surface.
#[test]
fn hidden_entry_points_parse_but_never_appear_in_help() {
    assert!(matches!(command(&["__complete"]), Command::Complete));
    assert!(matches!(command(&["mcp-await-job"]), Command::McpAwaitJob));
    match command(&["__api-key", "acme"]) {
        Command::ApiKey { profile } => assert_eq!(profile, "acme"),
        other => panic!("__api-key must parse, got {other:?}"),
    }
    assert!(matches!(command(&["run"]), Command::Run { .. }));

    let help = Cli::command().render_help().to_string();
    let long = Cli::command().render_long_help().to_string();
    for hidden in ["__complete", "__api-key", "mcp-await-job"] {
        assert!(
            !help.contains(hidden) && !long.contains(hidden),
            "{hidden} must stay out of both help surfaces"
        );
    }
    assert!(
        !help.contains("\n  run "),
        "the `run` redirect must stay out of the command table"
    );
}

/// Every command a user is meant to reach is in the root table, so a resync of
/// the docs or the completion scripts has one source to read.
#[test]
fn every_visible_subcommand_is_listed_in_the_root_help() {
    let help = Cli::command().render_help().to_string();
    for name in [
        "start",
        "login",
        "delete",
        "disable",
        "enable",
        "which",
        "sessions",
        "resume",
        "info",
        "daemon",
        "status",
        "mcp",
        "completions",
    ] {
        assert!(help.contains(name), "`{name}` must appear in the root help");
    }
}

// ── the exit-code contract ──────────────────────────────────────────────────

/// `-h` / `--help` / `-V` print and exit 0; a real parse failure exits 2. This
/// is clap's half of what `main` maps, so it is asserted through clap's own
/// `exit_code` rather than [`crate::exit_code`].
#[test]
fn help_and_version_exit_zero_while_parse_failures_exit_two() {
    for args in [
        ["-h"].as_slice(),
        ["--help"].as_slice(),
        ["-V"].as_slice(),
        ["--version"].as_slice(),
        ["start", "--help"].as_slice(),
    ] {
        let err = parse(args).expect_err("clap reports help/version as an Err");
        assert_eq!(err.exit_code(), 0, "{args:?} must exit 0");
    }
    for args in [["--bogus"].as_slice(), ["which", "extra"].as_slice()] {
        assert_eq!(parse_exit_code(args), 2, "{args:?} must exit 2");
    }
}

/// `clauth start --help` prints the subcommand's own prose, not the root block
/// — the whole point of moving the copy onto the variants.
#[test]
fn per_subcommand_help_carries_that_commands_prose() {
    let mut start = Cli::command();
    let start = start
        .find_subcommand_mut("start")
        .expect("start subcommand")
        .render_long_help()
        .to_string();
    assert!(
        start.contains("--isolated") && start.contains("--no-rescue"),
        "start --help must document its own flags"
    );
    assert!(
        start.contains("untouched"),
        "start --help must keep the passthrough prose"
    );
    assert!(
        !start.contains("completions"),
        "start --help must not reprint the root command list"
    );
}

/// A multi-word unrecognized invocation is a usage error (exit 2), not the old
/// help-plus-exit-0 that a calling script could not tell from success.
#[test]
fn an_unrecognized_multi_word_invocation_is_a_usage_error() {
    let Command::External(words) = command(&["strat", "acme"]) else {
        panic!("an unknown multi-word invocation lands on the external arm");
    };
    assert_eq!(words, ["strat", "acme"]);

    let err = dispatch(Cli {
        theme: None,
        command: Some(Command::External(vec!["strat".into(), "acme".into()])),
    })
    .expect_err("more than one bare word is nothing clauth knows");
    assert_eq!(crate::exit_code(Err(err)), 2);
}

/// `clauth daemon --status` with no daemon up is a plain failure (exit 1), not
/// a usage error — a spawner branches on the code alone.
#[test]
fn an_absent_daemon_reports_exit_one_not_the_usage_code() {
    let _home = crate::testutil::HomeSandbox::new();
    let err = dispatch(Cli {
        theme: None,
        command: Some(Command::Daemon {
            standby: false,
            no_standby: false,
            replace: false,
            status: true,
        }),
    })
    .expect_err("no daemon is running in the sandbox");
    assert!(
        err.to_string().contains("no clauth daemon is running"),
        "the failure must name the absence, not some incidental error: {err}"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
}

// ── completions: two positionals under one subcommand ───────────────────────

#[test]
fn completions_prints_for_a_shell_and_installs_with_an_optional_shell() {
    let Command::Completions { target, shell } = command(&["completions", "bash"]) else {
        panic!("must parse");
    };
    assert_eq!((target.as_str(), shell), ("bash", None));

    let Command::Completions { target, shell } = command(&["completions", "install"]) else {
        panic!("must parse");
    };
    assert_eq!((target.as_str(), shell), ("install", None));

    let Command::Completions { target, shell } = command(&["completions", "install", "zsh"]) else {
        panic!("must parse");
    };
    assert_eq!(target, "install");
    assert_eq!(shell.as_deref(), Some("zsh"));

    assert_eq!(parse_exit_code(&["completions"]), 2);
}

/// A second value after a shell name is a typo, not an install target.
#[test]
fn completions_rejects_a_second_value_after_a_shell_name() {
    let err = cmd_completions("bash", Some("extra")).expect_err("a stray second value must error");
    assert_eq!(crate::exit_code(Err(err)), 2);
    assert!(
        cmd_completions("powershell", None).is_err(),
        "an unsupported shell still routes to print_script's own rejection"
    );
}

// ── cmd_switch / cmd_start refuse a disabled target ─────────────────────────

mod disabled_target_refusal {
    use super::*;
    use crate::testutil::HomeSandbox;

    fn seed_disabled_profile(name: &str) {
        let mut config = crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        };
        crate::actions::create_blank_profile(&mut config, name.to_string(), None, None, None)
            .expect("create profile");
        crate::actions::disable_profile(&mut config, name).expect("disable profile");
    }

    #[test]
    fn cmd_switch_refuses_disabled_target_with_no_side_effects() {
        let _home = HomeSandbox::new();
        seed_disabled_profile("off");

        let err = cmd_switch("off").expect_err("a disabled target must be refused");
        assert_eq!(
            err.to_string(),
            "'off': account is disabled, run `clauth enable off`"
        );

        let reloaded = crate::profile::load_config().expect("reload");
        assert_eq!(
            reloaded.state.active_profile, None,
            "a refused switch must not change the active profile"
        );
    }

    #[test]
    fn cmd_start_refuses_disabled_target_before_acquiring_a_runtime() {
        let home = HomeSandbox::new();
        seed_disabled_profile("off");

        let err = cmd_start("off", &[], crate::runtime::Isolation::Shared, None)
            .expect_err("a disabled target must be refused");
        assert_eq!(
            err.to_string(),
            "'off': account is disabled, run `clauth enable off`"
        );

        assert!(
            !home
                .home()
                .join(".clauth")
                .join("profiles")
                .join("off")
                .join("runtime")
                .exists(),
            "the refusal must happen before any runtime is acquired"
        );
    }
}

// ── a bad profile name is a usage error, not a runtime failure ──────────────
// A typo'd subcommand is clap's `external` arm (dispatch routes it to
// `cmd_switch`); a typo'd profile name on `delete`/`start`/`disable`/`enable`
// reaches the same `resolve_or_bail`. Both should read as "you named something
// that isn't there" to a calling script: exit 2, distinguishable from success.
// Mirrors `main`'s parse -> dispatch -> exit_code mapping end-to-end.
mod bad_profile_name_is_a_usage_error {
    use super::*;
    use crate::testutil::HomeSandbox;

    fn dispatch_exit_code(args: &[&str]) -> i32 {
        let cli = parse(args).unwrap_or_else(|e| panic!("argv must parse: {e}"));
        crate::exit_code(crate::dispatch(cli))
    }

    #[test]
    fn a_bare_unknown_word_exits_2() {
        let _home = HomeSandbox::new();
        assert_eq!(
            dispatch_exit_code(&["strat"]),
            2,
            "a typo'd subcommand (a bare unknown word) is a usage error, not exit 1"
        );
    }

    #[test]
    fn delete_with_an_unknown_profile_exits_2() {
        let _home = HomeSandbox::new();
        assert_eq!(
            dispatch_exit_code(&["delete", "strat"]),
            2,
            "naming a profile that isn't there is a usage error, not exit 1"
        );
    }
}

// ── collect_api_endpoint: flag values get the prompt's trim + empty-reject ──
// Both flags present means no stdin read, so these run headless.

#[test]
fn collect_api_endpoint_trims_flag_values() {
    let (base, key) = collect_api_endpoint(Some("  https://api.x  "), Some("  sk-y  "))
        .expect("both flags present, no prompt");
    assert_eq!(base.as_deref(), Some("https://api.x"));
    assert_eq!(key.as_deref(), Some("sk-y"));
}

#[test]
fn collect_api_endpoint_rejects_empty_flag_values() {
    assert!(
        collect_api_endpoint(Some("   "), Some("sk")).is_err(),
        "a blank --base-url must bail, not create an empty-endpoint profile"
    );
    assert!(
        collect_api_endpoint(Some("https://x"), Some("")).is_err(),
        "a blank --api-key must bail, not store an empty key"
    );
}

#[test]
fn collect_api_endpoint_rejects_control_chars_in_key() {
    // The key is minted verbatim into a request header; a CRLF would inject one.
    assert!(
        collect_api_endpoint(Some("https://x"), Some("sk-a\r\nX-Evil: 1")).is_err(),
        "a control-char key must bail at capture, not persist a header-injecting value"
    );
    assert!(
        collect_api_endpoint(Some("https://x"), Some("sk a b")).is_err(),
        "interior whitespace in a key is a bad paste"
    );
}

// ── login_route: `clauth login <existing>` re-authenticates instead of bailing ──

fn config_with(names: &[&str]) -> crate::profile::AppConfig {
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    for n in names {
        config.add(crate::profile::Profile::new((*n).to_string(), None, None));
    }
    config
}

#[test]
fn login_route_new_name_creates() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "fresh"),
        LoginRoute::New("fresh".to_string())
    );
}

#[test]
fn login_route_existing_name_reauths() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "acme"),
        LoginRoute::Reauth("acme".to_string())
    );
}

// A case variant must land on the STORED canonical spelling — otherwise the
// collision validator would bail "already exists" and the reauth path is
// unreachable for anyone who types `ACME` for stored `acme` (the #7 report).
#[test]
fn login_route_case_variant_reauths_canonical_spelling() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "ACME"),
        LoginRoute::Reauth("acme".to_string())
    );
    assert_eq!(
        login_route(&config, "  acme  "),
        LoginRoute::Reauth("acme".to_string()),
        "surrounding whitespace is trimmed before matching"
    );
}

// The New arm must trim too, symmetric with Reauth: a stored `"  new  "` would
// be unreachable by the no-trim lookups every later command uses.
#[test]
fn login_route_new_name_trims_surrounding_whitespace() {
    let config = config_with(&["acme"]);
    assert_eq!(
        login_route(&config, "  fresh  "),
        LoginRoute::New("fresh".to_string())
    );
}

// Reauth overwrite confirm is default-NO: only an explicit y/yes proceeds.
#[test]
fn reauth_confirmed_only_on_explicit_yes() {
    for yes in ["y", "Y", "yes", "YES", "  y  ", "Yes\n"] {
        assert!(reauth_confirmed(yes), "{yes:?} should confirm");
    }
    for no in ["", "  ", "n", "no", "nope", "\n", "yeah", "ok"] {
        assert!(!reauth_confirmed(no), "{no:?} should decline");
    }
}

// ── hidden `clauth __api-key <profile>` (CC's apiKeyHelper body) ──────────────
//
// The hidden subcommand is what CC's `apiKeyHelper` runs per request to mint
// an auth value for an api-key profile (see `src/claude.rs`
// `build_claude_settings_json`). It reads the key from `config.toml` and
// prints it to stdout; on a missing profile or a profile with no api_key it
// fails closed with no stdout. The key never reaches argv (the helper command
// line carries only the profile name).

#[cfg(unix)]
mod api_key_helper_tests {
    use super::*;
    use crate::profile::save_profile;
    use crate::testutil::HomeSandbox;

    /// Write a profile to the sandboxed home with the given api_key (or none),
    /// then save it so `load_profile` can read it back the way the helper does.
    fn save_profile_with_key(name: &str, api_key: Option<&str>) {
        let mut profile = crate::testutil::blank_profile(name);
        profile.api_key = api_key.map(str::to_string);
        save_profile(&profile).expect("save_profile");
    }

    /// Dispatch a hidden `__api-key <profile>` the way `main` would.
    fn dispatch_api_key(profile: &str) -> Result<()> {
        dispatch(
            Cli::try_parse_from(["clauth", "__api-key", profile]).expect("hidden arm must parse"),
        )
    }

    /// `api_key_for_profile` returns the stored key verbatim for a profile that
    /// has one — the load path CC's helper relies on each request.
    #[test]
    fn api_key_for_profile_returns_stored_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("acme", Some("sk-test-12345"));
        let key = api_key_for_profile("acme").expect("load_profile");
        assert_eq!(key.as_deref(), Some("sk-test-12345"));
    }

    /// A profile that exists but has no api_key yields `Ok(None)`, which
    /// `cmd_api_key` turns into an Err (no stdout). This is the fail-closed
    /// path for a misconfigured helper.
    #[test]
    fn api_key_for_profile_none_when_profile_has_no_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("oauth-profile", None);
        let key = api_key_for_profile("oauth-profile").expect("load_profile");
        assert!(
            key.is_none(),
            "a profile with no api_key must yield Ok(None), not a blank Some"
        );
    }

    /// A missing profile surfaces as `Err`, not `Ok(None)` — so `cmd_api_key`
    /// fails for a helper string pointing at a profile name that no longer
    /// exists, rather than silently minting nothing.
    #[test]
    fn api_key_for_profile_err_for_missing_profile() {
        let _home = HomeSandbox::new();
        let err = api_key_for_profile("no-such-profile").expect_err("missing profile");
        assert!(
            err.to_string().contains("no-such-profile")
                || err.to_string().contains("failed to read"),
            "error must name the missing profile; got: {err}"
        );
    }

    /// A whitespace-only api_key reads as `None`: the helper must fail closed
    /// rather than emit a blank line CC would send as a credential. The trim
    /// also forgives a config.toml hand-edit with a trailing newline inside
    /// the quotes, which serde would otherwise preserve.
    #[test]
    fn api_key_for_profile_treats_blank_key_as_absent() {
        let _home = HomeSandbox::new();
        save_profile_with_key("blank", Some("   "));
        let key = api_key_for_profile("blank").expect("load_profile");
        assert!(key.is_none(), "a whitespace-only key must read as None");
    }

    /// End-to-end through `dispatch`: the helper dispatch arm reaches
    /// `cmd_api_key` for a profile that has a key and returns Ok (CC sees exit
    /// 0; the printed bytes are asserted separately by `write_api_key_*`
    /// below, since stdout can't be captured cleanly from a same-process
    /// `dispatch` call).
    #[test]
    fn dispatch_api_key_helper_returns_ok_for_profile_with_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("acme", Some("sk-dispatch-xyz"));
        dispatch_api_key("acme").expect("a profile with a key must exit 0");
    }

    /// `write_api_key` emits the key bytes VERBATIM with no trailing newline
    /// or other framing. CC's `apiKeyHelper` contract does not document
    /// trimming, so a trailing `\n` would be correct only under the unverified
    /// trim assumption; the bare-key form is correct either way. Pinned as a
    /// byte-exact assertion so a regression that reintroduces `println!`-style
    /// framing fails loudly instead of leaking a `\n`-suffixed credential.
    #[test]
    fn write_api_key_emits_no_trailing_newline() {
        let mut buf: Vec<u8> = Vec::new();
        write_api_key(&mut buf, "sk-test-12345").expect("write");
        assert_eq!(
            buf,
            b"sk-test-12345".to_vec(),
            "must emit exactly the key bytes — no newline, no framing"
        );

        // An empty-key call is structurally unreachable (`cmd_api_key` bails
        // before this fn on None), but the writer itself handles it without
        // inventing output.
        let mut empty: Vec<u8> = Vec::new();
        write_api_key(&mut empty, "").expect("write empty");
        assert!(empty.is_empty(), "an empty key writes zero bytes");
    }

    /// End-to-end through `dispatch`: the helper returns Err for a profile with
    /// no api_key — a helper pointing at a non-api profile must surface as a
    /// non-zero exit so CC's request fails loudly rather than sending a blank
    /// credential.
    #[test]
    fn dispatch_api_key_helper_fails_closed_for_profile_without_key() {
        let _home = HomeSandbox::new();
        save_profile_with_key("oauth-profile", None);
        dispatch_api_key("oauth-profile").expect_err("profile without a key must fail closed");
    }

    /// End-to-end through `dispatch`: the helper returns Err for a profile name
    /// that doesn't exist. A stale helper string from a deleted profile must
    /// surface as a request failure, not silently exit 0.
    #[test]
    fn dispatch_api_key_helper_fails_closed_for_missing_profile() {
        let _home = HomeSandbox::new();
        dispatch_api_key("ghost-profile").expect_err("missing profile must fail closed");
    }
}

// ── Fork flags: --new / --codex / --browser, and the fork verbs ─────────────

/// `--new` pins race-proof CREATE semantics for non-TTY callers; position-free
/// like every clap flag.
#[test]
fn login_new_flag_in_any_position() {
    assert!(login(&["login", "--new", "acme"]).new_only);
    assert!(login(&["login", "acme", "--new"]).new_only);
    assert!(!login(&["login", "acme"]).new_only);
}

/// `--codex` captures the live codex login; `--browser` only modifies it
/// (mints via PKCE instead of capturing).
#[test]
fn login_codex_flag_and_its_browser_modifier() {
    let a = login(&["login", "work", "--codex"]);
    assert!(a.codex);
    assert!(!a.browser);
    let b = login(&["login", "work", "--codex", "--browser"]);
    assert!(b.codex && b.browser);
}

/// Bare `--browser` is a usage error, not a synonym for the (always-browser)
/// claude login — the error names what it requires.
#[test]
fn login_bare_browser_is_refused() {
    let err = parse(&["login", "acme", "--browser"]).expect_err("bare --browser must be refused");
    assert_eq!(err.exit_code(), 2);
    assert!(
        err.to_string().contains("--codex"),
        "the error must name the required flag, got: {err}"
    );
}

/// The codex capture takes the live login verbatim — the claude-shaped flags
/// have no meaning there, and the sidecar capture is a different login again.
#[test]
fn login_codex_excludes_claude_shaped_flags() {
    for args in [
        ["login", "work", "--codex", "--model", "opus"].as_slice(),
        ["login", "work", "--codex", "--base-url", "https://x"].as_slice(),
        ["login", "work", "--codex", "--api-key", "sk-x"].as_slice(),
        ["login", "work", "--codex", "--setup-token"].as_slice(),
    ] {
        assert_eq!(parse_exit_code(args), 2, "{args:?} must be a usage error");
    }
}

/// `--codex --new` composes: a race-proof CREATE of a codex profile.
#[test]
fn login_codex_composes_with_new() {
    let a = login(&["login", "work", "--codex", "--new"]);
    assert!(a.codex && a.new_only);
}

/// The fork verbs parse: `feed`/`proxy`/`fallback` capture their rest verbatim
/// (each has its own grammar downstream), `doctor` is bare.
#[test]
fn fork_verbs_capture_their_rest_verbatim() {
    let Command::Feed { rest } = command(&["feed", "acme", "on"]) else {
        panic!("feed must parse");
    };
    assert_eq!(rest, ["acme", "on"]);
    let Command::Proxy { rest } = command(&["proxy", "--port", "4517"]) else {
        panic!("proxy must parse");
    };
    assert_eq!(rest, ["--port", "4517"]);
    let Command::Fallback { rest } = command(&["fallback", "threshold", "acme", "90"]) else {
        panic!("fallback must parse");
    };
    assert_eq!(rest, ["threshold", "acme", "90"]);
    assert!(matches!(command(&["doctor"]), Command::Doctor));
}
