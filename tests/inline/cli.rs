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
/// the TUI (on a terminal; a piped stdout prints the help instead — pinned
/// against the real binary in `tests/bare_non_tty.rs`, since the arm reads
/// `stdout().is_terminal()` live and an in-process pin would depend on the
/// runner's own terminal).
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
}

/// Where clauth's half of the grammar actually ends, pinned because it MOVED in
/// the clap port and the difference is silent. The hand-rolled parser stopped at
/// the profile name and forwarded every later token; clap keeps recognizing
/// `start`'s own flags past it, and only hands over on a token `start` does not
/// declare. `claude` has no `--isolated`/`--with-fallback`, so the only spelling
/// this reaches in practice is `--help`, and `--` forwards even that.
#[test]
fn clauths_own_start_flags_are_still_recognized_after_the_profile_name() {
    let Command::Start(a) = command(&["start", "acme", "--isolated"]) else {
        panic!("start must parse");
    };
    assert!(
        a.isolated,
        "clap keeps parsing start's own flags past the name"
    );
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
    assert_eq!(a.claude_args, ["-p", "hi"]);
}

/// `--with-fallback` is the whole opt-in: it is the only thing that sets a
/// registry row's `follows_chain`, and the decision leg is gated on that field.
/// Off by default, or landing the flag would move every already-running session
/// off the account it launched on.
#[test]
fn start_with_fallback_flag_parses_and_defaults_off() {
    let Command::Start(on) = command(&["start", "--with-fallback", "acme"]) else {
        panic!("must parse");
    };
    assert!(on.with_fallback, "the flag must reach StartArgs");
    assert_eq!(on.profile, "acme");

    let Command::Start(off) = command(&["start", "acme"]) else {
        panic!("must parse");
    };
    assert!(
        !off.with_fallback,
        "a bare start must not opt into the chain"
    );
}

/// An `--isolated` run gets a throwaway tree deliberately outside every chain,
/// and the swap executor refuses it at its own chokepoint. Rejected by the
/// parser so the user hears it instead of getting a flag that does nothing.
#[test]
fn start_with_fallback_conflicts_with_isolated() {
    for args in [
        ["start", "--with-fallback", "--isolated", "acme"].as_slice(),
        ["start", "--isolated", "--with-fallback", "acme"].as_slice(),
    ] {
        let err = parse(args).expect_err("--with-fallback with --isolated must be refused");
        assert_eq!(err.exit_code(), 2, "a bad flag combination exits 2");
        let text = err.to_string();
        assert!(
            text.contains("--with-fallback") && text.contains("--isolated"),
            "the error must name both flags, got: {err}"
        );
    }
}

/// Every isolated run rescues now, so the pair that used to decide it is gone
/// from the grammar. Refused, not accepted-and-ignored: a flag that parses and
/// changes nothing is how a `--no-rescue` in someone's shell alias keeps reading
/// as a working opt-out long after it stopped being one. `--isolated` is on the
/// second half because that was the accepted spelling, so it is the one a script
/// carries.
#[test]
fn start_no_longer_takes_the_removed_rescue_flags() {
    for args in [
        ["start", "--rescue", "acme"].as_slice(),
        ["start", "--no-rescue", "acme"].as_slice(),
        ["start", "--isolated", "--rescue", "acme"].as_slice(),
        ["start", "--isolated", "--no-rescue", "acme"].as_slice(),
    ] {
        let err = parse(args).expect_err("a removed rescue flag must be refused");
        assert_eq!(err.exit_code(), 2, "an unknown flag exits 2");
        assert!(
            err.to_string().contains("rescue"),
            "the error must name the flag it does not know, got: {err}"
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
        ["start", "--with-fallback"].as_slice(),
    ] {
        assert_eq!(
            parse_exit_code(args),
            2,
            "{args:?}: flags without a name must be a usage error"
        );
    }
}

/// `isolation()` is the whole of what `dispatch` derives from `StartArgs` before
/// handing it to `start::run`, and the runtime flavor it picks is what the
/// teardown's rescue leg reads.
#[test]
fn start_args_accessors_map_flags_to_the_runtime_types() {
    let shared = StartArgs {
        isolated: false,
        with_fallback: false,
        profile: "acme".into(),
        claude_args: Vec::new(),
    };
    assert_eq!(shared.isolation(), Isolation::Shared);

    let isolated = StartArgs {
        isolated: true,
        with_fallback: false,
        profile: "acme".into(),
        claude_args: Vec::new(),
    };
    assert_eq!(isolated.isolation(), Isolation::Isolated);
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

fn static_token(args: &[&str]) -> (String, bool, bool) {
    match command(args) {
        Command::StaticToken {
            profile,
            clear,
            yes,
        } => (profile, clear, yes),
        other => panic!("{args:?} must reach static-token, got {other:?}"),
    }
}

/// The clear half of the sidecar is a VERB, not a flag on `login`: it removes a
/// credential, and `login` is the command that adds one. The bare verb is the
/// restore (the inverse of `rolling-token`), `--clear` selects the removal, and
/// `--yes` belongs to the clear alone — the restore never prompts, so a `--yes`
/// beside the bare form would be accepted noise that later grew a meaning.
#[test]
fn static_token_bare_restores_and_clear_takes_an_unprompted_yes() {
    let (profile, clear, yes) = static_token(&["static-token", "acme"]);
    assert_eq!(profile, "acme");
    assert!(!clear, "the bare verb is the restore, not the clear");
    assert!(!yes);

    let (profile, clear, yes) = static_token(&["static-token", "acme", "--clear"]);
    assert_eq!(profile, "acme");
    assert!(clear);
    assert!(!yes);
    assert!(static_token(&["static-token", "acme", "--clear", "--yes"]).2);
    assert!(
        static_token(&["static-token", "acme", "--clear", "-y"]).2,
        "-y is the short spelling"
    );

    let err = parse(&["static-token", "acme", "--yes"])
        .expect_err("--yes without --clear must be refused");
    assert!(
        err.to_string().contains("--clear"),
        "the error must name what --yes requires, got: {err}"
    );
    assert_eq!(err.exit_code(), 2);
}

/// `login` no longer carries a clear flag, and its `--yes` means only "replace
/// the stored token unprompted" again.
#[test]
fn login_has_no_clear_flag_and_bare_yes_still_needs_setup_token() {
    let err = parse(&["login", "acme", "--clear-setup-token"])
        .expect_err("the retired flag must not parse");
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
        Command::Sessions {
            json: true,
            tokens: false
        }
    ));
    // The costly annotation is opt-in and independent of the output format.
    assert!(matches!(
        command(&["sessions", "--tokens"]),
        Command::Sessions {
            json: false,
            tokens: true
        }
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

/// `list` mirrors `status`'s hide-by-default posture: bare shows only enabled
/// profiles, `--all` and its `--disabled` alias each reveal the disabled ones.
/// Unlike `status` it has no required `--json` — the table is its only output.
#[test]
fn list_takes_all_and_disabled_flags_with_neither_required() {
    let Command::List { all, disabled } = command(&["list"]) else {
        panic!("bare `list` must parse");
    };
    assert!(!all && !disabled, "bare list reveals nothing");

    let Command::List { all, disabled } = command(&["list", "--all"]) else {
        panic!("must parse");
    };
    assert!(all && !disabled);

    let Command::List { all, disabled } = command(&["list", "--disabled"]) else {
        panic!("must parse");
    };
    assert!(
        !all && disabled,
        "--disabled is its own flag, ORed with --all"
    );

    let Command::List { all, disabled } = command(&["list", "--all", "--disabled"]) else {
        panic!("must parse");
    };
    assert!(all && disabled);

    assert_eq!(
        parse_exit_code(&["list", "--json"]),
        2,
        "list has no --json"
    );
    assert_eq!(
        parse_exit_code(&["list", "extra"]),
        2,
        "list takes no positional"
    );
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

/// The five internal entry points must still dispatch when invoked directly
/// (CC's `apiKeyHelper`, the bundled `asyncRewake` hook, the bundled
/// profile-change note hook, the bundled SessionStart self-heal hook, and the
/// completion scripts' name shellout all run them by name) while staying out
/// of every help surface. The spelling is the contract: `plugins/hooks/hooks.json`
/// invokes three of them by the exact string clap derives from the variant name.
#[test]
fn hidden_entry_points_parse_but_never_appear_in_help() {
    assert!(matches!(command(&["__complete"]), Command::Complete));
    assert!(matches!(command(&["mcp-await-job"]), Command::McpAwaitJob));
    assert!(matches!(
        command(&["hook-profile-changed-note"]),
        Command::HookProfileChangedNote
    ));
    assert!(matches!(command(&["self-heal"]), Command::SelfHeal));
    match command(&["__api-key", "acme"]) {
        Command::ApiKey { profile } => assert_eq!(profile, "acme"),
        other => panic!("__api-key must parse, got {other:?}"),
    }
    assert!(matches!(command(&["run"]), Command::Run { .. }));

    let help = Cli::command().render_help().to_string();
    let long = Cli::command().render_long_help().to_string();
    for hidden in [
        "__complete",
        "__api-key",
        "mcp-await-job",
        "hook-profile-changed-note",
        "self-heal",
    ] {
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

/// The bundled plugin manifest spells clauth subcommands as strings, and a
/// rename on either side is silent: the hook keeps being registered and just
/// exits non-zero on every fire. Derives both sides and compares, rather than
/// asserting one side's spelling.
#[test]
fn every_bundled_hook_command_parses_as_a_subcommand() {
    let manifest =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("plugins/hooks/hooks.json");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let json: serde_json::Value = serde_json::from_str(&text).expect("hooks.json must be JSON");

    let mut seen = 0;
    for (event, matchers) in json["hooks"]
        .as_object()
        .expect("hooks.json must carry a `hooks` object")
    {
        for entry in matchers.as_array().expect("each event holds an array") {
            for hook in entry["hooks"].as_array().expect("each entry holds hooks") {
                let command = hook["command"].as_str().expect("each hook has a command");
                let argv: Vec<&str> = command.split_whitespace().collect();
                assert_eq!(argv.first(), Some(&"clauth"), "{event}: {command}");
                parse(&argv[1..]).unwrap_or_else(|e| {
                    panic!("{event} runs `{command}`, which no longer parses: {e}")
                });
                seen += 1;
            }
        }
    }
    assert!(seen >= 4, "expected the bundled hooks, found {seen}");
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
        "list",
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
        start.contains("--isolated") && start.contains("--with-fallback"),
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

/// The one fact `--isolated`'s help has to carry now that every isolated run is
/// rescued: the runtime tree is thrown away, the session is not. No flag and no
/// config key says so any more, so this copy is where a reader learns it up
/// front — `sessions_cli::held_refusal` says it too, for the narrower case of a
/// resume blocked on a live isolated run. Matched over whitespace-normalized output,
/// because clap rewraps the paragraph to the terminal it renders for — and
/// without the final period, which clap strips off an arg's last sentence.
///
/// Narrowed 2026-08-20 from the whole sentence to its two load-bearing halves,
/// after the owner's copy pass reworded it. A full-sentence needle reds on every
/// rewrite regardless of whether the claim survived, which is the failure this
/// file's sibling pins already learned once.
///
/// The third needle is the HEDGE, and it is pinned as hard as the promise. The
/// lift runs in `start::run`'s teardown after `child.wait()` returns, so a hard
/// kill of `clauth start` itself skips it and the store goes with the runtime.
/// A help that promises the lift without that clause is the flat overclaim this
/// entry's reviewzy constraint was filed against.
#[test]
fn the_isolated_help_says_the_session_outlives_the_runtime() {
    let mut start = Cli::command();
    let help = start
        .find_subcommand_mut("start")
        .expect("start subcommand")
        .render_long_help()
        .to_string();
    let flat = help.split_whitespace().collect::<Vec<_>>().join(" ");
    for phrase in [
        "lifted into the global store",
        "stays resumable",
        "A hard kill skips that",
    ] {
        assert!(
            flat.contains(phrase),
            "--isolated's help must say the session survives its runtime \
             ({phrase:?}): {flat}"
        );
    }
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
        crate::actions::disable_profile(&mut config, &crate::profile::ProfileName::from(name))
            .expect("disable profile");
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

        let err = cmd_start("off", &[], crate::runtime::Isolation::Shared, false)
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
    let (base, key) = collect_api_endpoint(Some("  https://api.x  "), Some("  sk-y  "), false)
        .expect("both flags present, no prompt");
    assert_eq!(base.as_deref(), Some("https://api.x"));
    assert_eq!(key.as_deref(), Some("sk-y"));
}

#[test]
fn collect_api_endpoint_rejects_empty_flag_values() {
    assert!(
        collect_api_endpoint(Some("   "), Some("sk"), false).is_err(),
        "a blank --base-url must bail, not create an empty-endpoint profile"
    );
    assert!(
        collect_api_endpoint(Some("https://x"), Some(""), false).is_err(),
        "a blank --api-key must bail, not store an empty key"
    );
}

#[test]
fn collect_api_endpoint_rejects_control_chars_in_key() {
    // The key is minted verbatim into a request header; a CRLF would inject one.
    assert!(
        collect_api_endpoint(Some("https://x"), Some("sk-a\r\nX-Evil: 1"), false).is_err(),
        "a control-char key must bail at capture, not persist a header-injecting value"
    );
    assert!(
        collect_api_endpoint(Some("https://x"), Some("sk a b"), false).is_err(),
        "interior whitespace in a key is a bad paste"
    );
}

// ── api-mode reauth arm: pure routing pins ──────────────────────────────────
// The arm's two decisions are extracted pure (`resolve_reauth_base_url`,
// `api_reauth_snapshot`) so the routing is pinned without touching stdin: a
// test whose outcome depended on the runner's terminal red under a pty
// (the confirm prompt eats libtest's capture and declines) and hung on a
// developer terminal.

fn acme_with_chain() -> crate::profile::Profile {
    let mut acme = crate::profile::Profile::new(
        "acme".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-old".to_string()),
    );
    acme.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "stored-access".to_string(),
            refresh_token: Some("stored-refresh".to_string()),
            expires_at: None,
            scopes: None,
            subscription_type: None,
        }),
    });
    acme
}

#[test]
fn resolve_reauth_base_url_flag_wins_empty_included_and_ignores_the_terminal() {
    let acme = acme_with_chain();
    for tty in [false, true] {
        assert_eq!(
            resolve_reauth_base_url(Some("https://flag"), Some(&acme), tty).as_deref(),
            Some("https://flag"),
            "the flag wins over the stored endpoint, TTY or not"
        );
        assert_eq!(
            resolve_reauth_base_url(Some(""), Some(&acme), tty).as_deref(),
            Some(""),
            "an empty flag passes through; collect_api_endpoint's empty-reject turns it into the bail"
        );
    }
}

#[test]
fn resolve_reauth_base_url_a_tty_prompts() {
    let acme = acme_with_chain();
    assert_eq!(
        resolve_reauth_base_url(None, Some(&acme), true),
        None,
        "a TTY keeps the prompt: None lets collect_api_endpoint ask"
    );
}

#[test]
fn resolve_reauth_base_url_headless_reuses_the_stored_endpoint() {
    let acme = acme_with_chain();
    assert_eq!(
        resolve_reauth_base_url(None, Some(&acme), false).as_deref(),
        Some("https://api.deepseek.com/anthropic"),
        "a non-TTY re-key without --base-url takes the stored endpoint (owner ruling)"
    );
}

#[test]
fn resolve_reauth_base_url_headless_with_no_stored_endpoint_bails() {
    let bare = crate::profile::Profile::new("acme".to_string(), None, None);
    assert_eq!(
        resolve_reauth_base_url(None, Some(&bare), false),
        None,
        "None reaches collect_api_endpoint, whose non-interactive refusal fires"
    );
    assert_eq!(
        resolve_reauth_base_url(None, None, false),
        None,
        "a vanished profile reads the same as one with no endpoint stored"
    );
}

#[test]
fn api_reauth_snapshot_carries_the_stored_chain_through() {
    let acme = acme_with_chain();
    let snap = api_reauth_snapshot(
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-new".to_string()),
        Some(&acme),
    );
    let carried = snap
        .credentials
        .as_ref()
        .expect("the stored chain rides the snapshot");
    assert_eq!(carried.access_token(), Some("stored-access"));
    assert_eq!(carried.refresh_token(), Some("stored-refresh"));
    assert_eq!(
        carried.access_token(),
        acme.access_token(),
        "the carried chain is the stored profile's own, not a restated constant"
    );
    assert_eq!(
        snap.base_url.as_deref(),
        Some("https://api.deepseek.com/anthropic")
    );
    assert_eq!(snap.api_key.as_deref(), Some("sk-new"));
    assert_eq!(snap.account_uuid, None);
}

#[test]
fn api_reauth_snapshot_without_a_stored_chain_carries_none() {
    let bare = crate::profile::Profile::new(
        "acme".to_string(),
        Some("https://api.deepseek.com/anthropic".to_string()),
        Some("sk-old".to_string()),
    );
    assert!(
        api_reauth_snapshot(
            Some("https://x".to_string()),
            Some("sk-new".to_string()),
            Some(&bare)
        )
        .credentials
        .is_none(),
        "a profile with no chain contributes none"
    );
    assert!(
        api_reauth_snapshot(
            Some("https://x".to_string()),
            Some("sk-new".to_string()),
            None
        )
        .credentials
        .is_none(),
        "a vanished profile reads the same"
    );
}

// The composed helper, driving the real collect_api_endpoint so its
// validation actually fires. interactive = false throughout: the helper
// passes its own tty param through to collect_api_endpoint, so these pins
// exercise the non-interactive arms under ANY runner stdin (pinned under a
// pseudo-TTY, where the old stdin-keyed arm read the prompt leg instead).
// The interactive arms (prompt, read stdin) stay pinned at the router level
// only — driving them here would hang the suite.

#[test]
fn collect_api_reauth_snapshot_headless_reuses_endpoint_key_and_chain() {
    let acme = acme_with_chain();
    let snap = collect_api_reauth_snapshot(None, Some("sk-new"), Some(&acme), false)
        .expect("a headless re-key with a stored endpoint must not prompt");
    assert_eq!(
        snap.base_url.as_deref(),
        acme.base_url.as_deref(),
        "the snapshot carries the STORED endpoint"
    );
    assert_eq!(snap.api_key.as_deref(), Some("sk-new"), "and the fresh key");
    let carried = snap
        .credentials
        .as_ref()
        .expect("and the stored chain rides through the composition");
    assert_eq!(carried.access_token(), acme.access_token());
}

#[test]
fn collect_api_reauth_snapshot_headless_with_no_stored_endpoint_refuses() {
    let bare = crate::profile::Profile::new("acme".to_string(), None, None);
    let err = collect_api_reauth_snapshot(None, Some("sk-new"), Some(&bare), false)
        .expect_err("nothing to reuse and no way to prompt must refuse");
    assert!(
        err.to_string()
            .contains("non-interactive stdin: pass --base-url"),
        "the refusal must name the non-interactive bail: {err}"
    );
}

#[test]
fn collect_api_reauth_snapshot_empty_flag_refuses() {
    let acme = acme_with_chain();
    let err = collect_api_reauth_snapshot(Some(""), Some("sk-new"), Some(&acme), false)
        .expect_err("an empty --base-url must bail, not store an empty endpoint");
    assert!(
        err.to_string().contains("base url is required"),
        "the refusal must name the empty-reject: {err}"
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
        let mut profile = crate::testutil::blank_profile(&crate::profile::ProfileName::from(name));
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

/// `clauth herdr install` and its flags. The grammar is what makes the setup a
/// single command, so a rename or a dropped flag reds here rather than in a
/// user's shell.
#[test]
fn herdr_install_parses_with_every_flag() {
    let bare = command(&["herdr", "install"]);
    let Command::Herdr {
        cmd:
            crate::cli::HerdrCommand::Install {
                key,
                no_config,
                yes,
            },
    } = bare
    else {
        panic!("`herdr install` must select the install arm");
    };
    assert_eq!(
        key, None,
        "an omitted key is prompted for, never defaulted at the parser"
    );
    assert!(!no_config);
    assert!(!yes);

    let full = command(&[
        "herdr",
        "install",
        "--key",
        "prefix+a",
        "--no-config",
        "--yes",
    ]);
    let Command::Herdr {
        cmd:
            crate::cli::HerdrCommand::Install {
                key,
                no_config,
                yes,
            },
    } = full
    else {
        panic!("flagged form must select the install arm");
    };
    assert_eq!(key.as_deref(), Some("prefix+a"));
    assert!(no_config);
    assert!(yes);

    // `-y` is the short spelling every other confirm-gated command uses.
    let short = command(&["herdr", "install", "-y"]);
    let Command::Herdr {
        cmd: crate::cli::HerdrCommand::Install { yes, .. },
    } = short
    else {
        panic!("`-y` must select the install arm");
    };
    assert!(yes);

    // A bare `clauth herdr` names no operation, and `install` is not the only
    // one it will ever have, so it must stay a usage error rather than a default.
    assert!(parse(&["herdr"]).is_err());
    assert!(parse(&["herdr", "instal"]).is_err());
}

/// `clauth herdr uninstall` and its flags, mirroring the install grammar so the two stay siblings rather than drifting apart.
#[test]
fn herdr_uninstall_parses_with_every_flag() {
    let bare = command(&["herdr", "uninstall"]);
    let Command::Herdr {
        cmd: crate::cli::HerdrCommand::Uninstall { no_config, yes },
    } = bare
    else {
        panic!("`herdr uninstall` must select the uninstall arm");
    };
    assert!(!no_config);
    assert!(!yes);

    let full = command(&["herdr", "uninstall", "--no-config", "--yes"]);
    let Command::Herdr {
        cmd: crate::cli::HerdrCommand::Uninstall { no_config, yes },
    } = full
    else {
        panic!("flagged form must select the uninstall arm");
    };
    assert!(no_config);
    assert!(yes);

    let short = command(&["herdr", "uninstall", "-y"]);
    let Command::Herdr {
        cmd: crate::cli::HerdrCommand::Uninstall { yes, .. },
    } = short
    else {
        panic!("`-y` must select the uninstall arm");
    };
    assert!(yes);
}

/// The `static-token` verdicts, distinguished by what the sidecar HOLDS — a
/// profile already on its mint is a successful no-op, a rolling bearer left
/// with nothing re-stamping it is exit-non-zero, and an expired backup is
/// named rather than left for the operator to hunt down. This module is also
/// the exit-contract pin the review measured as missing: reverting any bail
/// to a print + `Ok(())` reds here.
mod static_token_verdicts {
    use super::*;
    use crate::testutil::HomeSandbox;

    fn seeded_profile(name: &str, rolling_flag: bool) {
        let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
        profile.rolling_token = rolling_flag;
        crate::profile::save_profile(&profile).expect("save profile");
        let state = crate::profile::AppState {
            profiles: vec![profile.name.clone()],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");
    }

    fn rolling_sidecar(name: &str, exp_in_ms: i64) {
        crate::claude::stamp_rolling_token(
            &crate::profile::ProfileName::from(name),
            &crate::profile::OAuthToken {
                access_token: "at-rolled".to_string(),
                refresh_token: None,
                expires_at: Some(crate::usage::now_ms() as i64 + exp_in_ms),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:profile".to_string(),
                ]),
                subscription_type: Some("max".into()),
            },
        )
        .expect("stamp");
    }

    #[test]
    fn a_mint_profile_is_a_noop_success() {
        let _home = HomeSandbox::new();
        seeded_profile("st-mint", false);
        crate::claude::write_session_token(
            &crate::profile::ProfileName::from("st-mint"),
            "sk-ant-oat01-static-verdicts-mint-000",
            crate::usage::now_ms() as i64,
        )
        .expect("mint");
        cmd_static_token("st-mint").expect("already on the mint is a no-op, not a failure");
    }

    #[test]
    fn a_rolling_bearer_with_no_backup_fails_loud() {
        let _home = HomeSandbox::new();
        seeded_profile("st-roll", true);
        rolling_sidecar("st-roll", 8 * 3_600_000);
        let err = cmd_static_token("st-roll")
            .expect_err("a bearer left with nothing re-stamping it is a failed restore");
        assert!(
            format!("{err:#}").contains("no live static mint to restore"),
            "{err:#}"
        );
        assert!(
            format!("{err:#}").contains("this command is what stopped its re-stamping"),
            "the copy owns the state the flag flip created, not just describes it: {err:#}"
        );
        // The flag flip IS durable on this path — stopping the re-stamps is
        // what the operator asked for; the missing mint is the error.
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("st-roll"))
            .expect("reload");
        assert!(
            !p.rolling_token,
            "the rolling flag turns off even when the restore fails"
        );
    }

    #[test]
    fn an_expired_backup_is_named_and_kept() {
        let _home = HomeSandbox::new();
        seeded_profile("st-aged", true);
        rolling_sidecar("st-aged", 8 * 3_600_000);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("st-aged"))
            .expect("dir");
        let expired = crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "sk-ant-oat01-aged".to_string(),
                refresh_token: None,
                expires_at: Some(crate::usage::now_ms() as i64 - 86_400_000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:sessions:claude_code".to_string(),
                ]),
                subscription_type: None,
            }),
        };
        std::fs::write(
            dir.join("session-token.static.json"),
            serde_json::to_vec_pretty(&expired).expect("ser"),
        )
        .expect("write backup");
        let err = cmd_static_token("st-aged").expect_err("an expired backup restores nothing");
        assert!(
            format!("{err:#}").contains("expired"),
            "the backup that exists but cannot serve is NAMED: {err:#}"
        );
        assert!(
            dir.join("session-token.static.json").exists(),
            "and left on disk"
        );
    }

    /// `rolling-token` on a quarantined chain bails BEFORE anything
    /// destructive: the sidecar (even a mis-filled one) is untouched and the
    /// flag stays off — a failed command leaves nothing durable behind.
    #[test]
    fn rolling_token_on_a_dead_chain_touches_nothing() {
        let _home = HomeSandbox::new();
        let mut profile = crate::profile::Profile::new("rt-dead".to_string(), None, None);
        profile.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-dead".to_string(),
                refresh_token: Some("rt-dead".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:profile".to_string(),
                ]),
                subscription_type: Some("max".into()),
            }),
        });
        crate::profile::save_profile(&profile).expect("save profile");
        let state = crate::profile::AppState {
            profiles: vec![profile.name.clone()],
            auth_broken: vec![profile.name.clone()],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");
        // A mis-filled sidecar that the pre-clear would have quarantined away.
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("rt-dead"))
            .expect("dir");
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec_pretty(&profile.credentials).expect("ser"),
        )
        .expect("write misfill");

        let err = cmd_rolling_token("rt-dead").expect_err("a dead chain refuses up front");
        assert!(
            format!("{err:#}").contains("usage chain is dead"),
            "{err:#}"
        );
        assert!(
            dir.join("session-token.json").exists(),
            "the mis-fill is NOT quarantined away by a command that then failed"
        );
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("rt-dead"))
            .expect("reload");
        assert!(!p.rolling_token, "nothing durable from a failed arm");
    }

    /// The reauth confirm's survivor clause is the prompt's only affirmative
    /// promise, and it is destructive to get wrong in either direction: a
    /// profile told its endpoint survives when the arm will not fire, or one
    /// told nothing survives while it silently keeps a key. It tracks the
    /// preserve arm's own predicate, so an OAuth profile — which has neither
    /// field — is never promised one.
    #[test]
    fn the_reauth_confirm_promises_a_survivor_only_where_one_survives() {
        assert_eq!(
            reauth_confirm_object(false, true),
            "stored subscription login, keeping its endpoint and api key"
        );
        assert_eq!(reauth_confirm_object(false, false), "stored credentials");
        // An api-mode login carries both fields and replaces them, so the
        // preserve never applies whatever the profile holds.
        assert_eq!(reauth_confirm_object(true, true), "endpoint + API key");
        assert_eq!(reauth_confirm_object(true, false), "endpoint + API key");
    }

    /// "Name the split state" (owner ruling, 2026-08-30): a quarantined
    /// third-party hybrid's dead chain sits beside a working api key, and the
    /// bail says so instead of prescribing the bare browser login.
    #[test]
    fn rolling_token_on_a_flagged_third_party_hybrid_names_the_split_state() {
        let _home = HomeSandbox::new();
        let mut profile = crate::profile::Profile::new(
            "rt-hybrid".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            Some("sk-live".to_string()),
        );
        profile.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-dead".to_string(),
                refresh_token: Some("rt-dead".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
                scopes: None,
                subscription_type: None,
            }),
        });
        crate::profile::save_profile(&profile).expect("save profile");
        let state = crate::profile::AppState {
            profiles: vec![profile.name.clone()],
            auth_broken: vec![profile.name.clone()],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");

        let err = cmd_rolling_token("rt-hybrid").expect_err("a flagged hybrid refuses up front");
        assert_eq!(
            format!("{err:#}"),
            "stored OAuth chain is dead, its api key still works: rt-hybrid (run \
             `clauth login rt-hybrid --api-key <key>` to clear the quarantine)"
        );

        // The keyless leg of the same bail. Its one reachable shape past the
        // load boundary: a key non-empty after trim (so `effective_base_url`
        // keeps the endpoint) that `validate_api_key` rejects — a hand-edited
        // `config.toml` or a bad paste, the `ds-ctrl` shape the MCP surface
        // fixtures. Without this the mirror can drift with nothing reddening.
        let mut unusable = crate::profile::Profile::new(
            "rt-badkey".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            Some("sk-test\r\nInjected: x".to_string()),
        );
        unusable.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-dead".to_string(),
                refresh_token: Some("rt-dead".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
                scopes: None,
                subscription_type: None,
            }),
        });
        crate::profile::save_profile(&unusable).expect("save profile");
        let state = crate::profile::AppState {
            profiles: vec![profile.name.clone(), unusable.name.clone()],
            auth_broken: vec![profile.name.clone(), unusable.name.clone()],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");
        #[allow(clippy::expect_used, reason = "test")]
        let reloaded =
            crate::profile::load_profile(&crate::profile::ProfileName::from("rt-badkey"))
                .expect("reload");
        assert!(
            reloaded.is_third_party() && !crate::claude::has_inference_auth(&reloaded),
            "fixture: the shape must survive the load boundary as keyless third-party",
        );

        let err = cmd_rolling_token("rt-badkey").expect_err("a flagged keyless hybrid refuses");
        assert_eq!(
            format!("{err:#}"),
            "profile has no api key: rt-badkey (run `clauth login rt-badkey --api-key <key>`)"
        );
    }

    /// "Copy-only: name why not" (owner ruling, 2026-08-30): an api-key
    /// third-party profile has no chain to roll, so the bail names that and
    /// prescribes nothing — a bare login would mint one, but that turns an
    /// api-key account into an Anthropic login rather than answering the
    /// request. An OAuth profile keeps the hint, where it IS the recovery.
    #[test]
    fn rolling_token_on_an_api_key_profile_names_the_missing_chain() {
        let _home = HomeSandbox::new();
        let ds = crate::profile::Profile::new(
            "rt-keyed".to_string(),
            Some("https://api.deepseek.com/anthropic".to_string()),
            Some("sk-live".to_string()),
        );
        crate::profile::save_profile(&ds).expect("save ds");
        let logged_out = crate::profile::Profile::new("rt-oauth".to_string(), None, None);
        crate::profile::save_profile(&logged_out).expect("save oauth");
        let state = crate::profile::AppState {
            profiles: vec![
                crate::profile::ProfileName::from("rt-keyed"),
                crate::profile::ProfileName::from("rt-oauth"),
            ],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");

        let err = cmd_rolling_token("rt-keyed").expect_err("no chain, no roll");
        assert_eq!(
            format!("{err:#}"),
            "'rt-keyed' has no usage OAuth chain to roll from"
        );

        let err = cmd_rolling_token("rt-oauth").expect_err("no chain either");
        assert!(
            format!("{err:#}").contains("run `clauth login rt-oauth` first"),
            "an OAuth profile keeps the recovery hint: {err:#}"
        );
    }

    /// A mint chain shape for the arm tests: a real access token whose grant
    /// was never recorded (setup scopes only, no plan stamp) — the shape
    /// `roll_from_stored_chain` refuses pre-stamp, so the arm fails AFTER the
    /// flag persist without touching the network.
    fn unrecorded_chain_profile(name: &str, rolling_flag: bool) {
        let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
        profile.rolling_token = rolling_flag;
        profile.credentials = Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-unrecorded".to_string(),
                refresh_token: Some("rt-unrecorded".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:sessions:claude_code".to_string(),
                ]),
                subscription_type: None,
            }),
        });
        crate::profile::save_profile(&profile).expect("save profile");
        let state = crate::profile::AppState {
            profiles: vec![profile.name.clone()],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");
    }

    /// The persist-before-arm shape's other half: a failed arm rolls the flag
    /// back to the value it FOUND — off stays off, and a flag that was
    /// already on is not stolen by the rollback. Reaches past the up-front
    /// pre-checks (the dead-chain test pins those) to the persist + rollback
    /// pair itself.
    #[test]
    fn a_failed_arm_rolls_the_flag_back_to_what_it_found() {
        let _home = HomeSandbox::new();
        unrecorded_chain_profile("rt-back", false);
        let err = cmd_rolling_token("rt-back").expect_err("an unrecorded grant refuses the arm");
        assert!(format!("{err:#}").contains("no recorded grant"), "{err:#}");
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("rt-back"))
            .expect("reload");
        assert!(
            !p.rolling_token,
            "the flag persisted for the arm is rolled back when the arm fails"
        );

        unrecorded_chain_profile("rt-keep", true);
        cmd_rolling_token("rt-keep").expect_err("same refusal");
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("rt-keep"))
            .expect("reload");
        assert!(
            p.rolling_token,
            "a flag that was already on is left on — rollback restores the PRIOR value, \
             never a hardcoded one"
        );
    }

    /// The read-back report refuses to claim success it cannot verify: no
    /// readable sidecar is an error, and a sidecar re-filled with a rotating
    /// pair mid-arm is an error — neither may decay into an `Ok(())` print.
    #[test]
    fn the_arming_report_fails_on_what_it_cannot_verify() {
        let _home = HomeSandbox::new();
        seeded_profile("rt-report", true);
        let err = report_armed_sidecar(&crate::profile::ProfileName::from("rt-report"), false)
            .expect_err("no readable sidecar must not report armed");
        assert!(
            format!("{err:#}").contains("no readable sidecar"),
            "{err:#}"
        );

        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("rt-report"))
            .expect("dir");
        let pair = crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at-raced".to_string(),
                refresh_token: Some("rt-raced".to_string()),
                expires_at: Some(crate::usage::now_ms() as i64 + 3_600_000),
                scopes: None,
                subscription_type: None,
            }),
        };
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec_pretty(&pair).expect("ser"),
        )
        .expect("write misfill");
        let err = report_armed_sidecar(&crate::profile::ProfileName::from("rt-report"), false)
            .expect_err("a raced-in rotating pair must not report armed");
        assert!(
            format!("{err:#}").contains("rotating pair while arming"),
            "{err:#}"
        );
    }

    /// "Already on its mint" is only a no-op success while the mint is ALIVE:
    /// an expired one signs sessions out on the next switch, which is a
    /// failed restore whatever the file layout says.
    #[test]
    fn an_expired_mint_in_the_sidecar_is_a_failed_restore() {
        let _home = HomeSandbox::new();
        seeded_profile("st-deadmint", false);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("st-deadmint"))
            .expect("dir");
        let dead = crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "sk-ant-oat01-clock-dead".to_string(),
                refresh_token: None,
                expires_at: Some(crate::usage::now_ms() as i64 - 1_000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:sessions:claude_code".to_string(),
                ]),
                subscription_type: None,
            }),
        };
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec_pretty(&dead).expect("ser"),
        )
        .expect("write sidecar");
        let err = cmd_static_token("st-deadmint").expect_err("an expired mint is not a no-op");
        assert!(format!("{err:#}").contains("EXPIRED"), "{err:#}");

        // Same verdict on the same grace the restore rule uses: a mint inside
        // Claude Code's five-minute refresh window is dead-on-arrival, and
        // identical bytes must not read as dead in the backup slot but fine
        // in the live one.
        seeded_profile("st-window", false);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("st-window"))
            .expect("dir");
        let closing = crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "sk-ant-oat01-two-minutes".to_string(),
                refresh_token: None,
                expires_at: Some(crate::usage::now_ms() as i64 + 2 * 60 * 1000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:sessions:claude_code".to_string(),
                ]),
                subscription_type: None,
            }),
        };
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec_pretty(&closing).expect("ser"),
        )
        .expect("write sidecar");
        let err = cmd_static_token("st-window")
            .expect_err("a mint inside CC's refresh window is not a no-op either");
        assert!(format!("{err:#}").contains("EXPIRED"), "{err:#}");
    }

    /// A restore that ERRORS (backup unreadable, not merely absent) exits
    /// after the flag flip already persisted, so the error must own that
    /// state the way the bail verdicts do — a raw filesystem error read as
    /// though the command had done nothing.
    #[test]
    fn a_failed_restore_error_owns_the_flag_it_flipped() {
        let _home = HomeSandbox::new();
        seeded_profile("st-eio", true);
        rolling_sidecar("st-eio", 8 * 3_600_000);
        let dir =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("st-eio")).expect("dir");
        // A directory where the backup goes: reads fail with a non-NotFound
        // error on every platform.
        std::fs::create_dir(dir.join("session-token.static.json")).expect("block the backup path");
        let err = cmd_static_token("st-eio").expect_err("an unreadable backup is loud");
        assert!(
            format!("{err:#}").contains("is off the rolling token now"),
            "the error owns the flag state the command already changed: {err:#}"
        );
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("st-eio"))
            .expect("reload");
        assert!(!p.rolling_token, "the flip it owns is real");
    }

    /// The trap a corrupt backup used to spring: `static-token` flipped the
    /// flag off, errored on the unparseable file, and the re-mint it
    /// prescribed ran with the flag off — the no-backup write path — so the
    /// file survived to fail the next attempt identically. The way out was
    /// `rolling-token`, re-arming the mode being left. Now the corrupt slot
    /// is quarantined (evidence kept under the profile) and the prescribed
    /// recovery actually recovers.
    #[test]
    fn a_corrupt_backup_is_quarantined_and_the_recovery_path_stays_open() {
        let _home = HomeSandbox::new();
        seeded_profile("st-corrupt", true);
        rolling_sidecar("st-corrupt", 8 * 3_600_000);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("st-corrupt"))
            .expect("dir");
        std::fs::write(dir.join("session-token.static.json"), b"not json at all")
            .expect("corrupt backup");

        let err = cmd_static_token("st-corrupt").expect_err("nothing restorable yet");
        assert!(
            format!("{err:#}").contains("no live static mint to restore"),
            "{err:#}"
        );
        assert!(
            !dir.join("session-token.static.json").exists(),
            "the corrupt slot-holder is cleared, not left to fail the next attempt"
        );
        let quarantined = std::fs::read_dir(dir.join("quarantine"))
            .expect("quarantine dir")
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".session-token.static.json")
            });
        assert!(quarantined, "the bytes survive as evidence");

        // The prescribed recovery: re-mint (the flag is off, so this is the
        // plain no-backup write — exactly what `clauth login --setup-token`
        // runs), then the command reports the mint as already in front.
        crate::claude::write_session_token(
            &crate::profile::ProfileName::from("st-corrupt"),
            "sk-ant-oat01-fresh-recovery-mint",
            crate::usage::now_ms() as i64,
        )
        .expect("re-mint");
        cmd_static_token("st-corrupt").expect("the recovery path converges instead of looping");
    }
}

/// The arm-report copy surfaces the round-6 review measured as silently
/// deletable or interchangeable, pinned as CONTENT (the print sites can no
/// longer lose them silently either — each fn has exactly one caller, so a
/// deleted print is a dead-code error under `-D warnings`).
mod armed_report_copy {
    use super::*;

    /// The four clear copy lines, pinned on CONTENT like every other line in
    /// this mod: single-caller keeps a deleted print a dead-code error, but
    /// only a pin keeps the WORDS — both pre-prompt lines are TTY-only, so no
    /// behavioral test ever executes them, and the two postscripts are the
    /// operator's only confirmation of what actually moved.
    #[test]
    fn the_clear_copy_names_the_disarm_the_backup_and_both_postscripts() {
        let disarm = clear_disarm_note("acme");
        assert!(
            disarm.contains("clearing also turns its re-stamping off"),
            "{disarm}"
        );
        let backup = clear_backup_note("acme");
        assert!(
            backup.contains("the preserved mint at session-token.static.json goes with it"),
            "{backup}"
        );
        assert!(
            backup.contains("`clauth static-token acme` will have nothing to restore"),
            "{backup}"
        );
        let disarmed = clear_disarmed_postscript("acme");
        assert!(disarmed.contains("rolling-token is off"), "{disarmed}");
        assert!(
            disarmed.contains("nothing re-stamps a sidecar for 'acme' now"),
            "{disarmed}"
        );
        let swept = clear_backup_postscript("acme");
        assert!(
            swept.contains("the preserved mint at session-token.static.json is gone"),
            "{swept}"
        );
        assert!(
            swept.contains("`clauth static-token acme` has nothing to restore now"),
            "{swept}"
        );
    }

    /// The post-clear report's GATING, pinned as a value since no stdout
    /// capture exists: each line rides exactly its own fact — the disarm line
    /// the flag, the backup line the removal — and a clear that moved neither
    /// reports neither. An unconditional backup line would tell every ordinary
    /// clear a year-scale credential was destroyed when none existed.
    #[test]
    fn the_clear_postscripts_ride_exactly_what_moved() {
        assert!(clear_postscripts("acme", false, false).is_empty());
        let disarm_only = clear_postscripts("acme", true, false);
        assert_eq!(disarm_only.len(), 1, "{disarm_only:?}");
        assert!(disarm_only[0].contains("rolling-token is off"));
        let backup_only = clear_postscripts("acme", false, true);
        assert_eq!(backup_only.len(), 1, "{backup_only:?}");
        assert!(backup_only[0].contains("is gone"));
        let both = clear_postscripts("acme", true, true);
        assert_eq!(both.len(), 2, "{both:?}");
        assert!(
            both[0].contains("rolling-token is off") && both[1].contains("is gone"),
            "{both:?}"
        );
    }

    /// The disclosure is the feature's entire security posture in user-facing
    /// copy — there is no confirm prompt by design — so the widening, its
    /// consequence, and the way back must each survive verbatim.
    #[test]
    fn the_scope_widening_disclosure_names_the_widening_and_the_way_back() {
        let line = scope_widening_disclosure("acme");
        assert!(
            line.contains("wider than the setup-token mint it supersedes"),
            "{line}"
        );
        assert!(
            line.contains("Anything that can read this profile's session credential"),
            "{line}"
        );
        assert!(
            line.contains("`clauth static-token acme` puts the mint back"),
            "{line}"
        );
    }

    /// Each health state makes its OWN claim: `Fresh` promising while no
    /// daemon runs reads the arm as durable when nothing will re-stamp it.
    #[test]
    fn each_daemon_health_state_makes_its_own_restamp_claim() {
        let absent = restamp_promise(crate::daemon::DaemonHealth::Absent);
        let stale = restamp_promise(crate::daemon::DaemonHealth::Stale);
        let fresh = restamp_promise(crate::daemon::DaemonHealth::Fresh);
        assert!(
            absent.contains("No daemon appears to be running"),
            "{absent}"
        );
        assert!(absent.contains("`clauth daemon` starts"), "{absent}");
        assert!(stale.contains("looks stale"), "{stale}");
        assert!(stale.contains("`clauth daemon --status`"), "{stale}");
        assert!(fresh.contains("re-stamps it before it expires"), "{fresh}");
        assert!(
            !fresh.contains("No daemon") && !fresh.contains("stale"),
            "the healthy claim carries no warning language: {fresh}"
        );
    }

    /// The warning for a failed arm whose rollback also failed to save: the
    /// only line telling the operator a durable flag-on-with-nothing-armed
    /// state exists. It owns the state, carries the save error, and names the
    /// exit.
    #[test]
    fn the_stranded_rollback_warning_owns_the_flag_and_the_exit() {
        let w = rollback_stranded_warning("acme", &anyhow::anyhow!("read-only file system"));
        assert!(
            w.contains("could not roll the rolling-token flag back for 'acme'"),
            "{w}"
        );
        assert!(w.contains("read-only file system"), "{w}");
        assert!(w.contains("`clauth static-token acme` to clear it"), "{w}");
    }
}

/// `static-token --clear` as the FULL exit from the long-lived token: all three
/// pieces of that state go together (sidecar, preserved mint, `rolling_token`
/// flag), because each one left behind resurrects what the operator was told is
/// gone — a lingering flag has the daemon re-stamp a fresh sidecar, and a
/// lingering backup keeps the actual year-scale credential on disk under a
/// command that just printed "cleared".
mod static_token_clear {
    use super::*;
    use crate::testutil::HomeSandbox;

    /// A profile with a stored OAuth pair (so the other-login guard passes),
    /// the rolling flag as given, saved into app state.
    fn cleared_profile(name: &str, rolling_flag: bool, with_login: bool) {
        let mut profile = crate::profile::Profile::new(name.to_string(), None, None);
        profile.rolling_token = rolling_flag;
        if with_login {
            profile.credentials = Some(crate::profile::ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "at-usage".to_string(),
                    refresh_token: Some("rt-usage".to_string()),
                    expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                    scopes: None,
                    subscription_type: None,
                }),
            });
        }
        crate::profile::save_profile(&profile).expect("save profile");
        let state = crate::profile::AppState {
            profiles: vec![profile.name.clone()],
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("save state");
    }

    #[test]
    fn clear_takes_the_sidecar_the_backup_and_the_flag_together() {
        let _home = HomeSandbox::new();
        cleared_profile("cl-roll", true, true);
        // A mint first, then the rolling stamp: the first stamp preserves the
        // mint into `session-token.static.json`, which is exactly the two-file
        // state a rolling profile carries in production.
        crate::claude::write_session_token(
            &crate::profile::ProfileName::from("cl-roll"),
            "sk-ant-oat01-clear-full-exit-mint",
            crate::usage::now_ms() as i64,
        )
        .expect("mint");
        crate::claude::stamp_rolling_token(
            &crate::profile::ProfileName::from("cl-roll"),
            &crate::profile::OAuthToken {
                access_token: "at-rolled".to_string(),
                refresh_token: None,
                expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                scopes: Some(vec![
                    "user:inference".to_string(),
                    "user:profile".to_string(),
                ]),
                subscription_type: Some("max".into()),
            },
        )
        .expect("stamp");
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-roll"))
            .expect("dir");
        assert!(dir.join("session-token.static.json").exists(), "fixture");

        cmd_static_token_clear("cl-roll", true).expect("the clear succeeds");

        assert!(
            !dir.join("session-token.json").exists(),
            "the sidecar is gone"
        );
        assert!(
            !dir.join("session-token.static.json").exists(),
            "the preserved mint is a long-lived credential and goes with the clear"
        );
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("cl-roll"))
            .expect("reload");
        assert!(
            !p.rolling_token,
            "the flag goes too, or the daemon re-stamps a sidecar over the clear"
        );
    }

    /// The other-login refusal is re-checked UNDER the rotation guard, not only
    /// on the pre-prompt snapshot: the prompt is an unbounded wait, and a
    /// log-out (the TUI's row, or `rm credentials.json` by hand) can land
    /// inside it — the one interleaving where this command strips a profile's
    /// last credential. Driven here through the guard itself: the test holds
    /// the profile's rotation lock, deletes the stored login while the clear is
    /// parked on `acquire`, and only then releases.
    ///
    /// Green is deliberately timing-proof (WHICHEVER check catches the state,
    /// nothing may be stripped); the sleep only makes the under-guard check the
    /// one that fires, which is what the mutation sweep measures.
    #[test]
    fn the_other_login_refusal_is_rechecked_under_the_guard() {
        let home = HomeSandbox::new();
        cleared_profile("cl-race", false, true);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-race"))
            .expect("dir");

        // One attempt of the race. NOTHING between the guard acquire and the
        // join may panic: an unwound attempt would detach the worker, release
        // the flock, and drop the sandbox out from under a thread that then
        // resolves the REAL home — the exact hazard `HomeSandbox`'s drop-order
        // doc exists for. Every fallible step in the window reports through
        // the return value instead.
        let attempt = || -> (bool, String) {
            crate::claude::write_session_token(
                &crate::profile::ProfileName::from("cl-race"),
                "sk-ant-oat01-clear-race-mint0000",
                crate::usage::now_ms() as i64 + 300 * 24 * 3_600_000,
            )
            .expect("mint");
            std::fs::write(
                dir.join("credentials.json"),
                serde_json::to_vec(&crate::profile::ClaudeCredentials {
                    claude_ai_oauth: Some(crate::profile::OAuthToken {
                        access_token: "at-race".to_string(),
                        refresh_token: Some("rt-race".to_string()),
                        expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                        scopes: None,
                        subscription_type: None,
                    }),
                })
                .expect("serialize login"),
            )
            .expect("store the login");
            let guard = crate::runtime::RotationGuard::acquire(&crate::profile::ProfileName::from(
                "cl-race",
            ))
            .expect("hold the lock");
            let worker = std::thread::spawn(move || super::cmd_static_token_clear("cl-race", true));
            // Give the clear time to pass its pre-guard snapshot and park on
            // the guard; then take the login away and let it through.
            std::thread::sleep(std::time::Duration::from_millis(250));
            let logged_out = std::fs::remove_file(dir.join("credentials.json")).is_ok();
            drop(guard);
            let result = worker.join().expect("the clear thread survives");
            assert!(logged_out, "the fixture's login vanished before the race");
            // WHICHEVER check catches the state, nothing may be stripped —
            // green is timing-proof even when the worker loses the 250ms race
            // and its pre-guard check fires instead.
            let err = result.expect_err("clearing the last credential must refuse");
            assert!(
                format!("{err:#}").contains("stores no other login"),
                "the refusal names the reason: {err:#}"
            );
            assert!(
                dir.join("session-token.json").exists(),
                "a refused clear removes nothing"
            );
            // "anymore" appears ONLY in the under-guard re-check's message —
            // the discriminator that says the race was actually won.
            (
                format!("{err:#}").contains("stores no other login anymore"),
                format!("{err:#}"),
            )
        };

        // The under-guard leg is what this test is FOR, so retry a lost race
        // instead of silently degrading into a second pin of the pre-guard
        // check: five parked-thread attempts losing 250ms each is not a
        // plausible machine, it is a broken re-check.
        let mut last = String::new();
        for _ in 0..5 {
            let (under_guard, err) = attempt();
            last = err;
            if under_guard {
                break;
            }
        }
        assert!(
            last.contains("stores no other login anymore"),
            "the UNDER-GUARD re-check never fired across five attempts: {last}"
        );
        drop(home);
    }

    /// The preserved mint goes LAST, after the relink: a backup-removal
    /// failure between the sidecar removal and the relink would leave an
    /// ACTIVE profile's live slot a dangling symlink under a bare "remove
    /// failed" — a broken login reported as nothing-happened. Driven by
    /// blocking the backup slot with a directory: the sidecar clears, the
    /// relink lands, and only then does the removal fail — with a context
    /// line owning the partial state.
    #[test]
    fn the_clear_relinks_before_the_backup_removal_can_fail() {
        let home = HomeSandbox::new();
        cleared_profile("cl-mid", false, true);
        crate::claude::write_session_token(
            &crate::profile::ProfileName::from("cl-mid"),
            "sk-ant-oat01-clear-mid-mint00000",
            crate::usage::now_ms() as i64 + 300 * 24 * 3_600_000,
        )
        .expect("mint");
        let dir =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-mid")).expect("dir");
        let state = crate::profile::AppState {
            profiles: vec!["cl-mid".into()],
            active_profile: Some("cl-mid".into()),
            ..Default::default()
        };
        crate::profile::save_app_state(&state).expect("activate");
        crate::claude::force_link_profile_credentials(&crate::profile::ProfileName::from("cl-mid"))
            .expect("link");
        let live = home.home().join(".claude").join(".credentials.json");
        assert_eq!(
            std::fs::read_link(&live).expect("live is a symlink"),
            dir.join("session-token.json"),
            "fixture: the live slot starts on the sidecar"
        );
        std::fs::create_dir(dir.join("session-token.static.json")).expect("block the slot");

        let err = cmd_static_token_clear("cl-mid", true)
            .expect_err("the blocked backup slot fails the removal");
        assert!(
            format!("{err:#}").contains("the preserved mint at session-token.static.json remains"),
            "the partial state is owned in the error: {err:#}"
        );
        assert!(
            !dir.join("session-token.json").exists(),
            "the sidecar clear itself succeeded"
        );
        assert_eq!(
            std::fs::read_link(&live).expect("live survives as a symlink"),
            dir.join("credentials.json"),
            "the relink landed BEFORE the backup removal failed — no dangling live slot"
        );
    }

    /// The widened nothing-to-clear gate: a set flag with NO files is exactly
    /// the state where an early "nothing to clear" would leave the daemon to
    /// re-create what the operator was told is gone.
    #[test]
    fn a_set_flag_with_no_files_is_still_something_to_clear() {
        let _home = HomeSandbox::new();
        cleared_profile("cl-flag", true, true);
        cmd_static_token_clear("cl-flag", true).expect("the disarm succeeds");
        let p = crate::profile::load_profile(&crate::profile::ProfileName::from("cl-flag"))
            .expect("reload");
        assert!(!p.rolling_token, "the clear turned re-stamping off");
    }

    /// The other-login guard covers the backup slot: a preserved mint is
    /// restorable by the bare verb, so destroying it when it is the profile's
    /// only credential strips the profile the same way removing the sidecar
    /// would.
    #[test]
    fn the_backup_slot_counts_as_the_last_credential() {
        let _home = HomeSandbox::new();
        cleared_profile("cl-last", false, false);
        let dir = crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-last"))
            .expect("dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // A live mint in the backup slot and nothing else: the one credential.
        crate::claude::write_session_token(
            &crate::profile::ProfileName::from("cl-last"),
            "sk-ant-oat01-last-credential-mint",
            crate::usage::now_ms() as i64,
        )
        .expect("mint");
        std::fs::rename(
            dir.join("session-token.json"),
            dir.join("session-token.static.json"),
        )
        .expect("move into the backup slot");

        let err = cmd_static_token_clear("cl-last", true)
            .expect_err("clearing the only credential is refused");
        assert!(
            format!("{err:#}").contains("stores no other login"),
            "{err:#}"
        );
        assert!(
            dir.join("session-token.static.json").exists(),
            "a refused clear removes nothing"
        );
    }

    /// With all three pieces absent the clear is a quiet no-op success, not an
    /// error — the requested end state already holds. The early return is
    /// pinned by a side-effect the fall-through path cannot avoid: the full
    /// path acquires the rotation guard, which materializes this profile's
    /// lock file, so its absence is what proves the no-op branch ran rather
    /// than a false "cleared" printing through the whole body. The path is
    /// asked for rather than spelled — the lock lives outside the profile
    /// directory, and a hand-built profile-dir path would assert the absence
    /// of something that is never there whichever branch ran.
    #[test]
    fn nothing_to_clear_is_a_noop_success() {
        let _home = HomeSandbox::new();
        cleared_profile("cl-none", false, true);
        cmd_static_token_clear("cl-none", true).expect("nothing to clear is success");
        assert!(
            !crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from("cl-none"))
                .expect("rotation lock path")
                .exists(),
            "the no-op branch returns before the rotation guard — a lock file \
             here means the full clear body ran against nothing"
        );
    }

    /// A mis-filled sidecar is EVIDENCE — the anomaly the split exists to
    /// detect, and one the operator did not name (the prompt says "the
    /// long-lived token"; a rotating pair is precisely not that). The clear
    /// quarantines it before removal, like every other path that disposes of
    /// one.
    #[test]
    fn clear_quarantines_a_misfilled_sidecar_instead_of_plain_deleting_it() {
        let _home = HomeSandbox::new();
        cleared_profile("cl-mf", false, true);
        let dir =
            crate::profile::profile_dir(&crate::profile::ProfileName::from("cl-mf")).expect("dir");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("session-token.json"),
            serde_json::to_vec(&crate::profile::ClaudeCredentials {
                claude_ai_oauth: Some(crate::profile::OAuthToken {
                    access_token: "at-misfill".to_string(),
                    refresh_token: Some("rt-misfill".to_string()),
                    expires_at: Some(crate::usage::now_ms() as i64 + 8 * 3_600_000),
                    scopes: None,
                    subscription_type: None,
                }),
            })
            .expect("ser"),
        )
        .expect("write misfill");

        cmd_static_token_clear("cl-mf", true).expect("the clear succeeds");

        assert!(!dir.join("session-token.json").exists(), "sidecar removed");
        let quarantined: Vec<_> = std::fs::read_dir(dir.join("quarantine"))
            .expect("quarantine dir exists")
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "the rotating pair is moved aside as evidence, never plain-deleted"
        );
    }
}

/// The CLI delete refuses while a rotation holds the profile's lock, driven
/// through the real grammar so the guard is taken for the name `cmd_delete`
/// RESOLVED rather than the raw argument.
///
/// Pinned at this surface because nothing else can: `cmd_delete` holds no
/// ranked lock, so no `debug_assert` watches its ordering, and a guard taken
/// for the wrong profile leaves every other test in the tree green.
#[test]
fn cli_delete_refuses_while_a_rotation_holds_the_lock() {
    let _home = crate::testutil::HomeSandbox::new();

    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(&mut config, "cli-held".to_string(), None, None, None)
        .expect("create profile");

    // Another process mid-rotation: a locked handle on a separate fd.
    let lock_path =
        crate::runtime::rotation_lock_path(&crate::profile::ProfileName::from("cli-held"))
            .expect("rotation lock path");
    crate::profile::mkdir_700(lock_path.parent().expect("lock parent")).expect("locks dir");
    let holder = crate::profile::open_state_file(&lock_path).expect("open holder handle");
    holder.lock().expect("hold the rotation lock");

    // Spelled in a case `canonical_name` has to fold, so the argument and the
    // resolved name DIFFER: guarding the raw argument locks a path nothing
    // holds, and this is the only fixture shape that can tell the two apart.
    // `--yes` only skips the confirm prompt; it is not a rotation override.
    let outcome = dispatch(
        Cli::try_parse_from(["clauth", "delete", "CLI-HELD", "--yes"]).expect("delete must parse"),
    );

    // Untouched state first: a guard taken for the wrong profile deletes the
    // account, and asserting the error first would abort before these run.
    assert!(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("cli-held"))
            .expect("profile dir")
            .exists(),
        "a refused delete leaves the profile directory on disk"
    );
    assert!(
        load_config()
            .expect("reload state")
            .find(&crate::profile::ProfileName::from("cli-held"))
            .is_some(),
        "a refused delete leaves the profile record in the persisted state"
    );
    assert_eq!(
        outcome
            .expect_err("an in-flight rotation must block the CLI delete")
            .to_string(),
        "'cli-held' has a token rotation in progress, retry in a moment"
    );
}

// ── login --manual: the browser-free OAuth login ─────────────────────────────

#[test]
fn login_manual_flag_parses_bare_and_composes_with_model() {
    let a = login(&["login", "acme", "--manual"]);
    assert!(a.manual);
    assert!(!a.is_api_mode());
    assert!(!a.setup_token);
    assert!(!login(&["login", "acme"]).manual, "off unless asked");
    assert_eq!(
        login(&["login", "acme", "--manual", "--model", "opus"])
            .model
            .as_deref(),
        Some("opus")
    );
}

/// `--manual` is an OAuth login; the api-key pair and the setup-token sidecar
/// are different credentials, so each combination is a contradiction.
#[test]
fn login_manual_excludes_api_mode_and_setup_token() {
    for extra in [
        &["--base-url", "https://x"][..],
        &["--api-key", "k"][..],
        &["--setup-token"][..],
    ] {
        let mut args = vec!["login", "acme", "--manual"];
        args.extend_from_slice(extra);
        let err = parse(&args).expect_err("must be refused");
        assert!(
            err.to_string().contains("cannot be used with"),
            "{extra:?} must read as a conflict, got: {err}"
        );
        assert_eq!(err.exit_code(), 2);
    }
}

/// The piped-stdin reader behind `--manual`: a driver writes one line and may
/// close stdin without a newline; nothing longer than the cap is ever held.
#[test]
fn read_manual_code_from_accepts_one_bounded_line() {
    use std::io::Cursor;
    let ok = |s: &str| super::read_manual_code_from(Cursor::new(s.as_bytes().to_vec()));
    assert_eq!(ok("abc#st\n").expect("newline-terminated").trim(), "abc#st");
    assert_eq!(ok("abc#st").expect("eof-terminated").trim(), "abc#st");
    assert!(
        ok("abc#st\nsecond line\n").expect("first line only").trim() == "abc#st",
        "only the first line is the code"
    );
    let err = ok("").expect_err("immediate eof");
    assert!(err.to_string().contains("no code on stdin"), "{err}");
    let err = ok("   \n").expect_err("blank line");
    assert!(err.to_string().contains("no code on stdin"), "{err}");
    // The cap is judged on the code, not the line: exactly the cap passes
    // with a `\n`, a CRLF, or EOF behind it, and one byte more is refused
    // however the line ends.
    let exact = "a".repeat(crate::oauth_login::MANUAL_CODE_MAX);
    for tail in ["\n", "\r\n", ""] {
        let got = ok(&format!("{exact}{tail}")).unwrap_or_else(|e| panic!("{tail:?}: {e}"));
        assert_eq!(got.trim_end_matches(['\r', '\n']), exact, "{tail:?}");
    }
    let long = "a".repeat(crate::oauth_login::MANUAL_CODE_MAX + 1);
    for tail in ["\n", "\r\n", ""] {
        let err = ok(&format!("{long}{tail}")).expect_err("one over the cap");
        assert!(err.to_string().contains("longer"), "{tail:?}: {err}");
        assert!(!err.to_string().contains("aaaa"), "never echoes the input");
    }
}

/// `--manual` on an Alibaba Model Studio profile is refused before anything
/// runs: the profile's login is its console session, so the flag has no flow
/// to select. The refusal sits ahead of `confirm_reauth`, which is what lets
/// this drive `cmd_login` in-process: no TTY prompt, no browser, no network.
#[test]
fn cmd_login_manual_refuses_an_alibaba_profile_before_any_flow() {
    let _home = crate::testutil::HomeSandbox::new();
    let mut config = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: Vec::new(),
    };
    crate::actions::create_blank_profile(
        &mut config,
        "ali".to_string(),
        Some("https://token-plan.ap-southeast-1.maas.aliyuncs.com/apps/anthropic".to_string()),
        Some("sk-sp-test".to_string()),
        None,
    )
    .expect("create profile");
    assert_eq!(
        crate::profile::load_config()
            .expect("reload")
            .find(&crate::profile::ProfileName::from("ali"))
            .and_then(|p| p.provider),
        Some(crate::providers::Provider::Alibaba),
        "the seed must read back as an Alibaba profile"
    );

    let args = login(&["login", "ali", "--manual"]);
    let err = cmd_login(args).expect_err("--manual on an Alibaba profile must be refused");
    let msg = err.to_string();
    assert!(msg.contains("--manual"), "{msg}");
    assert!(msg.contains("Alibaba"), "{msg}");
    assert!(msg.contains("clauth login ali"), "names the repair:\n{msg}");

    let reloaded = crate::profile::load_config().expect("reload");
    let p = reloaded
        .find(&crate::profile::ProfileName::from("ali"))
        .expect("profile survives");
    assert_eq!(
        p.api_key.as_deref(),
        Some("sk-sp-test"),
        "nothing was overwritten"
    );
}
