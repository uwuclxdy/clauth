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

// ── clauth switch: one verb, two forms split by arity ───────────────────────

/// One positional is the global form (the bare-word act under its own verb),
/// two positionals the session form — arity alone decides, so a sid-shaped
/// first value is never guessed at.
#[test]
fn switch_splits_the_forms_on_arity_alone() {
    let Command::Switch { name, profile } = command(&["switch", "acme"]) else {
        panic!("one positional must parse as the global form");
    };
    assert_eq!(name, "acme");
    assert_eq!(profile, None, "one positional is the global form");

    let Command::Switch { name, profile } = command(&["switch", "4242-0"]) else {
        panic!("a sid-shaped single name still parses as the global form");
    };
    assert_eq!(name, "4242-0");
    assert_eq!(
        profile, None,
        "arity alone decides, never the first value's shape"
    );

    let Command::Switch { name, profile } = command(&["switch", "4242-0", "spare"]) else {
        panic!("two positionals must parse as the session form");
    };
    assert_eq!(name, "4242-0");
    assert_eq!(profile.as_deref(), Some("spare"));

    let err = parse(&["switch", "a", "b", "c"]).expect_err("three positionals is a usage error");
    assert_eq!(err.exit_code(), 2);
}

/// `start` hands `claude` everything after the profile byte-identically,
/// leading hyphens included, so a passthrough `-p`/`--model` is never eaten as
/// a clauth flag.
#[test]
fn start_forwards_claude_args_verbatim_including_leading_hyphens() {
    let Command::Start(a) = command(&["start", "acme", "-p", "hi", "--model", "opus"]) else {
        panic!("start must parse");
    };
    assert_eq!(a.profile.as_deref(), Some("acme"));
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
    assert_eq!(a.profile.as_deref(), Some("acme"));
    assert_eq!(
        a.claude_args,
        ["--model", "haiku"],
        "the separator is clap's, the args behind it are claude's"
    );
}

// ── --auto ──────────────────────────────────────────────────────────

/// `--auto` defers the account to selection and takes the profile's place, so
/// there is no name to read back.
#[test]
fn start_auto_defers_the_account_and_keeps_the_positional_free() {
    let Command::Start(a) = command(&["start", "--auto"]) else {
        panic!("start --auto must parse with no profile");
    };
    assert!(a.auto);
    assert_eq!(a.target(), crate::cli::StartTarget::Auto);
    assert!(a.passthrough().is_empty());
}

/// The trap `passthrough` exists for: clap still binds the first trailing value
/// to the (now unused) profile slot, so without the fold `claude` would receive
/// `hi` and lose the `-p` in front of it.
#[test]
fn start_auto_folds_the_positional_back_into_claude_args() {
    let Command::Start(a) = command(&["start", "--auto", "--", "-p", "hi", "--model", "opus"])
    else {
        panic!("start must parse");
    };
    assert_eq!(
        a.passthrough(),
        ["-p", "hi", "--model", "opus"],
        "--auto leaves no profile, so nothing may be eaten as one"
    );
}

/// A non-hyphen first argument needs no separator, and is folded the same way.
#[test]
fn start_auto_folds_a_bare_first_argument_too() {
    let Command::Start(a) = command(&["start", "--auto", "do the thing"]) else {
        panic!("start must parse");
    };
    assert_eq!(a.target(), crate::cli::StartTarget::Auto);
    assert_eq!(a.passthrough(), ["do the thing"]);
}

/// Why the `--` is documented rather than worked around: with no name in the
/// profile slot, clap has nothing to tell a passthrough `-p` from a misspelled
/// clauth flag, so it refuses instead of guessing. Pinned so the day someone
/// makes the positional take hyphen values, this says what it costs.
#[test]
fn start_auto_refuses_a_bare_hyphen_argument_without_a_separator() {
    parse(&["start", "--auto", "-p", "hi"])
        .expect_err("a leading-hyphen claude arg after --auto needs the `--` separator");
}

/// Without `--auto` the positional is the profile and nothing is folded.
#[test]
fn start_without_auto_keeps_the_named_target() {
    let Command::Start(a) = command(&["start", "acme", "-p", "hi"]) else {
        panic!("start must parse");
    };
    assert_eq!(
        a.target(),
        crate::cli::StartTarget::Named("acme".to_owned())
    );
    assert_eq!(a.passthrough(), ["-p", "hi"]);
}

/// A bare `start` still has to name an account.
#[test]
fn start_without_auto_requires_a_profile() {
    parse(&["start"]).expect_err("a start with neither a profile nor --auto must be refused");
}

/// `--auto` and `--with-fallback` compose: pick the best entry point, then let
/// the chain rescue it if that account runs out.
#[test]
fn start_auto_composes_with_with_fallback() {
    let Command::Start(a) = command(&["start", "--auto", "--with-fallback"]) else {
        panic!("start must parse");
    };
    assert!(a.auto && a.with_fallback);
    assert_eq!(a.target(), crate::cli::StartTarget::Auto);
}

// ── start's own flags ───────────────────────────────────────────────────────

#[test]
fn start_isolated_flag_precedes_the_name() {
    let Command::Start(a) = command(&["start", "--isolated", "acme", "-p", "hi"]) else {
        panic!("start must parse");
    };
    assert_eq!(a.profile.as_deref(), Some("acme"));
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
    assert_eq!(on.profile.as_deref(), Some("acme"));

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
        auto: false,
        explain: false,
        profile: Some("acme".into()),
        claude_args: Vec::new(),
    };
    assert_eq!(shared.isolation(), Isolation::Shared);

    let isolated = StartArgs {
        isolated: true,
        with_fallback: false,
        auto: false,
        explain: false,
        profile: Some("acme".into()),
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

// ── capture ──────────────────────────────────────────────────────────────────

#[test]
fn capture_parses_with_a_profile_argument() {
    let Command::Capture { profile } = command(&["capture", "acme"]) else {
        panic!("capture must parse");
    };
    assert_eq!(profile, "acme");
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

/// `--cert`/`--key` come as a pair, and only alongside `--listen`.
///
/// Both halves matter. A lone `--cert` would otherwise be accepted and then
/// silently fall back to the lego certificate at startup, which is the failure
/// the flag exists to avoid — the operator would be told the host has no
/// certificate for a name they never asked it to use. And without `--listen`
/// there is no listener for either file to serve, so accepting them would be a
/// no-op that reads like configuration.
#[test]
fn cert_and_key_are_required_together_and_only_with_listen() {
    for args in [
        ["daemon", "--listen", "--cert", "/tmp/a.crt"].as_slice(),
        ["daemon", "--listen", "--key", "/tmp/a.key"].as_slice(),
        ["daemon", "--cert", "/tmp/a.crt", "--key", "/tmp/a.key"].as_slice(),
    ] {
        assert_eq!(parse_exit_code(args), 2, "{args:?} must be a usage error");
    }

    let Command::Daemon {
        listen, cert, key, ..
    } = command(&[
        "daemon",
        "--listen",
        "--cert",
        "/tmp/a.crt",
        "--key",
        "/tmp/a.key",
    ])
    else {
        panic!("must parse");
    };
    assert!(
        listen.is_some(),
        "bare --listen still takes the default bind"
    );
    assert_eq!(
        (cert.as_deref(), key.as_deref()),
        (
            Some(std::path::Path::new("/tmp/a.crt")),
            Some(std::path::Path::new("/tmp/a.key"))
        )
    );
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
        listen,
        cert,
        key,
        dump_openapi,
    } = command(&["daemon"])
    else {
        panic!("must parse");
    };
    assert_eq!(
        (standby, no_standby, replace, status),
        (false, false, false, false),
        "bare `clauth daemon` picks no mode, which dispatch reads as exit-if-running"
    );
    assert!(
        !dump_openapi,
        "the document dump is opt-in exactly like the listener"
    );
    assert_eq!(
        listen, None,
        "the REST API is off unless an address is asked for"
    );
    assert_eq!(
        (cert, key),
        (None, None),
        "TLS comes from this host's lego certificate unless both files are named"
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
            listen,
            cert: _,
            key: _,
            dump_openapi: _,
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
        assert_eq!(listen, None, "{args:?} asks for no listener");
    }

    // Every pair conflicts, so no invocation can ask for two start modes, and
    // the one-shot `--status` cannot be asked for alongside one.
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

/// `--listen` is orthogonal to the start modes (a supervised daemon still
/// serves the API) but not to the one-shots, which print and exit.
#[test]
fn listen_parses_an_address_and_composes_with_the_start_modes() {
    let Command::Daemon { listen, .. } = command(&["daemon", "--listen", "0.0.0.0:8443"]) else {
        panic!("must parse");
    };
    assert_eq!(
        listen,
        Some(std::net::SocketAddr::from(([0, 0, 0, 0], 8443))),
        "clap parses the address, so a typo fails before the daemon starts"
    );

    let Command::Daemon { listen, .. } = command(&["daemon", "--listen", "127.0.0.1:9000"]) else {
        panic!("must parse");
    };
    assert_eq!(
        listen,
        Some(std::net::SocketAddr::from(([127, 0, 0, 1], 9000)))
    );

    for mode in ["--standby", "--no-standby", "--replace"] {
        let Command::Daemon { listen, .. } = command(&["daemon", mode, "--listen", "0.0.0.0:8443"])
        else {
            panic!("daemon {mode} --listen must parse");
        };
        assert!(listen.is_some(), "{mode} should not conflict with --listen");
    }

    assert_eq!(
        parse_exit_code(&["daemon", "--status", "--listen", "0.0.0.0:8443"]),
        2,
        "daemon --status --listen must be refused as a conflict"
    );

    for bad in ["8443", "not-an-address", "0.0.0.0", "0.0.0.0:99999"] {
        assert_eq!(
            parse_exit_code(&["daemon", "--listen", bad]),
            2,
            "--listen {bad:?} must be refused at parse time"
        );
    }
}

/// A value-less `--listen` binds [`crate::cli::DEFAULT_LISTEN`], and taking no
/// value must not make the flag start swallowing the argument after it.
#[test]
fn bare_listen_defaults_to_every_interface_without_eating_the_next_flag() {
    let default = crate::cli::DEFAULT_LISTEN
        .parse::<std::net::SocketAddr>()
        .expect("DEFAULT_LISTEN must be a parseable address");

    let Command::Daemon { listen, .. } = command(&["daemon", "--listen"]) else {
        panic!("bare `daemon --listen` must parse");
    };
    assert_eq!(
        listen,
        Some(default),
        "a value-less --listen takes the documented default"
    );

    // The flag is still opt-in: nothing about the default leaks into a `daemon`
    // that never asked for a listener.
    let Command::Daemon { listen, .. } = command(&["daemon"]) else {
        panic!("must parse");
    };
    assert_eq!(listen, None, "no --listen still means no listener");

    // `num_args = 0..=1` is the risk here: a following flag must be read as a
    // flag, not consumed as the address, in either order.
    for mode in ["--standby", "--no-standby", "--replace"] {
        let Command::Daemon {
            listen,
            standby,
            no_standby,
            replace,
            ..
        } = command(&["daemon", "--listen", mode])
        else {
            panic!("daemon --listen {mode} must parse");
        };
        assert_eq!(
            listen,
            Some(default),
            "--listen {mode} must default, not swallow {mode}"
        );
        assert!(
            standby || no_standby || replace,
            "{mode} must still register as a start mode after a bare --listen"
        );

        let Command::Daemon { listen, .. } = command(&["daemon", mode, "--listen"]) else {
            panic!("daemon {mode} --listen must parse");
        };
        assert_eq!(
            listen,
            Some(default),
            "{mode} then a bare --listen defaults"
        );
    }

    // The one-shot conflicts with the shorthand exactly as it does with the
    // spelled-out address.
    assert_eq!(
        parse_exit_code(&["daemon", "--status", "--listen"]),
        2,
        "daemon --status --listen must be refused as a conflict"
    );
}

/// The global token's flags are gone with no shim, so a script still passing
/// either gets clap's own unknown-argument error, alone or beside another flag.
#[test]
fn the_retired_token_flags_are_unknown_arguments() {
    for args in [
        ["daemon", "--print-token"].as_slice(),
        ["daemon", "--rotate-token"].as_slice(),
        ["daemon", "--listen", "--print-token"].as_slice(),
        ["daemon", "--status", "--rotate-token"].as_slice(),
    ] {
        let err = parse(args).expect_err("a retired flag must not parse");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::UnknownArgument,
            "{args:?}"
        );
        assert_eq!(err.exit_code(), 2, "{args:?}");
    }
}

/// `--dump-openapi` writes the exact bytes `GET /api/v1/openapi.json` serves.
/// Driven through `write_openapi_document` into a buffer, never the real
/// stdout: the document is ~27 KB and printing it on every selecting run is
/// noise. The no-home pin and the gone-reader exit live in
/// `tests/dump_openapi.rs`, which spawns the real binary.
#[test]
fn dump_openapi_writes_the_served_bytes_verbatim() {
    let mut buf: Vec<u8> = Vec::new();
    crate::write_openapi_document(&mut buf).expect("dump must write");
    assert_eq!(
        buf,
        crate::daemon::api::routes::openapi_document_bytes().expect("document serializes"),
        "the dump must be the exact bytes GET /api/v1/openapi.json serves"
    );
}

/// A reader that left mid-dump ends the dump at `Ok` — exit 0 at the real
/// entry — because the pipeline reported what the reader returned, not this run
/// failing.
#[test]
fn dump_openapi_ends_ok_when_the_reader_is_gone() {
    struct BrokenPipe;
    impl std::io::Write for BrokenPipe {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    crate::write_openapi_document(&mut BrokenPipe)
        .expect("a gone reader ends the dump at Ok, not an error");
}

/// `--dump-openapi` refuses every flag that starts or probes a daemon, so no
/// invocation can ask for both a document dump and a listener/certificate/probe.
#[test]
fn dump_openapi_conflicts_with_every_daemon_starting_or_probing_flag() {
    for (other, value) in [
        ("--standby", None),
        ("--no-standby", None),
        ("--replace", None),
        ("--status", None),
        ("--listen", None),
        ("--cert", Some("/tmp/a.crt")),
        ("--key", Some("/tmp/a.key")),
    ] {
        let mut args = vec!["daemon", "--dump-openapi", other];
        if let Some(v) = value {
            args.push(v);
        }
        let err = parse(&args).expect_err("--dump-openapi must refuse this flag");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "--dump-openapi + {other}"
        );
        assert_eq!(err.exit_code(), 2, "--dump-openapi + {other}");
    }
}

// ── devices ─────────────────────────────────────────────────────────────────

/// `devices` parses its five verbs: bare lists, with or without `--json`;
/// `pair` and `add` take a name and an optional `--control` plus
/// `--control`-gated `--sessions`; `revoke` and `allow-sessions` a name.
#[test]
fn devices_parses_its_five_verbs() {
    use crate::cli::DevicesCommand;

    assert!(matches!(
        command(&["devices"]),
        Command::Devices {
            json: false,
            cmd: None
        }
    ));
    assert!(matches!(
        command(&["devices", "--json"]),
        Command::Devices {
            json: true,
            cmd: None
        }
    ));
    for (args, want_control, want_sessions) in [
        (["devices", "pair", "phone"].as_slice(), false, false),
        (
            ["devices", "pair", "phone", "--control"].as_slice(),
            true,
            false,
        ),
        (
            ["devices", "pair", "--control", "phone"].as_slice(),
            true,
            false,
        ),
        (
            ["devices", "pair", "phone", "--control", "--sessions"].as_slice(),
            true,
            true,
        ),
    ] {
        let Command::Devices {
            cmd:
                Some(DevicesCommand::Pair {
                    name,
                    control,
                    sessions,
                }),
            ..
        } = command(args)
        else {
            panic!("{args:?} must parse as pair");
        };
        assert_eq!(
            (name.as_str(), control, sessions),
            ("phone", want_control, want_sessions),
            "{args:?}"
        );
    }
    for (args, want_control, want_sessions) in [
        (["devices", "add", "tray"].as_slice(), false, false),
        (
            ["devices", "add", "tray", "--control"].as_slice(),
            true,
            false,
        ),
        (
            ["devices", "add", "tray", "--control", "--sessions"].as_slice(),
            true,
            true,
        ),
    ] {
        let Command::Devices {
            cmd:
                Some(DevicesCommand::Add {
                    name,
                    control,
                    sessions,
                }),
            ..
        } = command(args)
        else {
            panic!("{args:?} must parse as add");
        };
        assert_eq!(
            (name.as_str(), control, sessions),
            ("tray", want_control, want_sessions),
            "{args:?}"
        );
    }
    let Command::Devices {
        cmd: Some(DevicesCommand::Revoke { name }),
        ..
    } = command(&["devices", "revoke", "phone"])
    else {
        panic!("revoke must parse");
    };
    assert_eq!(name, "phone");

    let Command::Devices {
        cmd: Some(DevicesCommand::AllowSessions { name }),
        ..
    } = command(&["devices", "allow-sessions", "phone"])
    else {
        panic!("allow-sessions must parse");
    };
    assert_eq!(name, "phone");

    for args in [
        ["devices", "pair"].as_slice(),
        ["devices", "add"].as_slice(),
        ["devices", "revoke"].as_slice(),
        ["devices", "allow-sessions"].as_slice(),
        ["devices", "revoke", "phone", "--control"].as_slice(),
        ["devices", "pair", "phone", "extra"].as_slice(),
        ["devices", "--json", "pair", "phone"].as_slice(),
        ["devices", "pair", "phone", "--json"].as_slice(),
        ["devices", "list"].as_slice(),
        ["devices", "pair", "phone", "--sessions"].as_slice(),
        ["devices", "add", "tray", "--sessions"].as_slice(),
    ] {
        assert_eq!(parse_exit_code(args), 2, "{args:?} must be a usage error");
    }
}

/// `revoke` of a name no device holds is a plain failure naming it: exit 1,
/// not the usage code.
#[test]
fn revoking_an_unknown_device_exits_one_naming_it() {
    let _home = crate::testutil::HomeSandbox::new();
    let err = dispatch(Cli {
        theme: None,
        command: Some(Command::Devices {
            json: false,
            cmd: Some(crate::cli::DevicesCommand::Revoke {
                name: "ghost".to_string(),
            }),
        }),
    })
    .expect_err("no device holds that name");
    assert_eq!(
        err.to_string(),
        "no device named 'ghost'; `clauth devices` lists the paired ones"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);
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
    assert!(matches!(
        command(&["__complete"]),
        Command::Complete {
            codex: false,
            live_sessions: false
        }
    ));
    assert!(matches!(
        command(&["__complete", "--codex"]),
        Command::Complete {
            codex: true,
            live_sessions: false
        }
    ));
    assert!(matches!(
        command(&["__complete", "--live-sessions"]),
        Command::Complete {
            codex: false,
            live_sessions: true
        }
    ));
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
        "switch",
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

/// The bare `clauth <profile>` act is deprecated in favour of `clauth switch
/// <name>` — said in the help and the wiki, never as a runtime warning on the
/// most-used path.
#[test]
fn the_bare_profile_form_is_deprecated_in_the_help() {
    let help = Cli::command().render_help().to_string();
    assert!(
        help.contains("deprecated, use `clauth switch <name>`"),
        "the root help must name the replacement for the bare form: {help}"
    );
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
            listen: None,
            cert: None,
            key: None,
            dump_openapi: false,
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

    /// The one-name form reaches the exact function the bare word reaches, so
    /// its refusals are byte-identical — pinned through the dispatch seam, not
    /// by calling `cmd_switch` directly.
    #[test]
    fn switch_refuses_a_disabled_target_through_dispatch() {
        let _home = HomeSandbox::new();
        seed_disabled_profile("off");

        let cli = parse(&["switch", "off"]).expect("one positional parses as the global form");
        let err = crate::dispatch(cli).expect_err("a disabled target must be refused");
        assert_eq!(
            err.to_string(),
            "'off': account is disabled, run `clauth enable off`",
            "the refusal copy is the bare form's, byte for byte"
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

        let err = cmd_start(
            &crate::cli::StartTarget::Named("off".to_owned()),
            &[],
            crate::runtime::Isolation::Shared,
            false,
            false,
        )
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

    #[test]
    fn cmd_start_explain_refuses_a_disabled_target() {
        let _home = HomeSandbox::new();
        seed_disabled_profile("off");

        let err = cmd_start(
            &crate::cli::StartTarget::Named("off".to_owned()),
            &[],
            crate::runtime::Isolation::Shared,
            false,
            true,
        )
        .expect_err("explain must run the same refusals as a launch");
        assert_eq!(
            err.to_string(),
            "'off': account is disabled, run `clauth enable off`"
        );
    }
}

// ── a bad profile name is a usage error, not a runtime failure ──────────────
// A typo'd subcommand is clap's `external` arm (dispatch routes it to
// `cmd_switch`); a typo'd profile name on `disable`/`enable`/`rolling-token`/
// `static-token` reaches `resolve_or_bail` while `switch`/`delete`/`start`
// resolve by hand, so either reads as "you named something that isn't there"
// to a calling script: exit 2, distinguishable from success.
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

    /// `clauth switch <name>` is the bare-word act under its own verb, so an
    /// unknown name is the same usage error through the same seam.
    #[test]
    fn switch_with_an_unknown_name_exits_2() {
        let _home = HomeSandbox::new();
        assert_eq!(
            dispatch_exit_code(&["switch", "strat"]),
            2,
            "an unknown name on the one-arg form is a usage error, not exit 1"
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
            ..crate::profile::OAuthToken::default_extra()
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
// The hidden subcommand is what CC's `apiKeyHelper` runs to obtain an auth
// value for an api-key profile (see `src/claude.rs`
// `build_claude_settings_json`). It reads the key from `config.toml` and
// prints it to stdout; on a missing profile or a profile with no api_key it
// fails closed with no stdout. The key never reaches argv (the helper command
// line carries only the profile name). The value is the profile's stored
// STATIC key — the helper reads, never mints, so the same bytes come back on
// every call until a re-login or the divergence adopt re-captures the key.

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

    /// Two consecutive loads return the SAME key: the helper is a pure reader
    /// with no per-call minting or rotation, so the value handed to a child
    /// session stays valid — the "token survives a child session" half of the
    /// api-key surface contract. A rotation introduced here reds this test
    /// while the stored-key pin above still passes.
    #[test]
    fn api_key_for_profile_is_static_across_calls() {
        let _home = HomeSandbox::new();
        save_profile_with_key("acme", Some("sk-test-12345"));
        let first = api_key_for_profile("acme").expect("load_profile");
        let second = api_key_for_profile("acme").expect("reload");
        assert_eq!(
            first, second,
            "the helper must return the stored key verbatim on every call, \
             never a rotated or single-use value"
        );
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
    /// exists, rather than silently printing nothing.
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                    ..crate::profile::OAuthToken::default_extra()
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
                ..crate::profile::OAuthToken::default_extra()
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
                        ..crate::profile::OAuthToken::default_extra()
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
                    ..crate::profile::OAuthToken::default_extra()
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
    let _holder = crate::testutil::hold_rotation_lock("cli-held");

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

/// The not-found listing names BOTH rosters — `switch` and `delete` accept
/// codex names, so a list hiding them turns a typo'd codex name into "no such
/// thing" — with the codex half labeled so nothing reads as a claude profile.
#[test]
fn the_not_found_listing_names_both_rosters() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    std::fs::write(clauth.join("codex-profiles.toml"), "profiles = [\"cx1\"]\n")
        .expect("write codex state");
    let config = crate::profile::AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["cl1".into()],
            ..Default::default()
        },
        profiles: vec![crate::testutil::blank_profile(
            &crate::profile::ProfileName::from("cl1"),
        )],
    };

    let msg = unknown_profile_error(&config, "ghost").to_string();
    assert!(msg.contains("profile 'ghost' not found"), "{msg}");
    assert!(msg.contains("cl1"), "claude roster listed: {msg}");
    assert!(
        msg.contains("codex: cx1"),
        "codex roster listed, labeled: {msg}"
    );

    // An empty claude roster leaves no dangling separator.
    let empty = crate::profile::AppConfig {
        state: crate::profile::AppState::default(),
        profiles: vec![],
    };
    let msg = unknown_profile_error(&empty, "ghost").to_string();
    assert!(msg.contains("available: codex: cx1"), "{msg}");
}

/// `--with-fallback` refuses ON a codex profile by name, before any spawn:
/// codex reads `auth.json` once at start, so a chain lands at the NEXT start
/// rather than mid-session, and silently not doing what the flag promises is
/// the worse answer. (`--rescue`/`--no-rescue` were the other half until
/// upstream retired them; an isolated session keeps its transcripts now.)
#[test]
fn codex_start_refuses_with_fallback_by_name() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    std::fs::write(clauth.join("codex-profiles.toml"), "profiles = [\"cx\"]\n")
        .expect("write codex state");

    let err = cmd_start(
        &crate::cli::StartTarget::Named("cx".to_owned()),
        &[],
        Isolation::Shared,
        true,
        false,
    )
    .expect_err("--with-fallback refuses on codex");
    assert!(err.to_string().contains("--with-fallback"), "{err}");
    assert!(err.to_string().contains("NEXT start"), "{err}");
}

/// `cmd_delete_codex` takes the rotation guard exactly where `cmd_delete`
/// does — after the confirm gate, before the delete — so a codex name typed at
/// the CLI under an in-flight rotation refuses with the same words and touches
/// nothing. Spelled in a case `canonical_name` folds, like the claude twin.
#[test]
fn cli_codex_delete_refuses_while_a_rotation_holds_the_lock() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    std::fs::write(
        clauth.join("codex-profiles.toml"),
        "active_profile = \"cx-held\"\nprofiles = [\"cx-held\"]\n",
    )
    .expect("write codex state");
    crate::testutil::write_codex_store("cx-held", "{}");

    let _holder = crate::testutil::hold_rotation_lock("cx-held");

    let outcome = dispatch(
        Cli::try_parse_from(["clauth", "delete", "CX-HELD", "--yes"]).expect("delete must parse"),
    );

    assert!(
        crate::profile::profile_dir(&crate::profile::ProfileName::from("cx-held"))
            .expect("profile dir")
            .join("auth.json")
            .exists(),
        "a refused delete leaves the store on disk"
    );
    let state = crate::codex_profiles::CodexState::load().expect("reload codex state");
    assert!(
        state.holds("cx-held"),
        "a refused delete leaves the roster entry"
    );
    assert_eq!(state.active_profile().map(|n| n.as_str()), Some("cx-held"));
    assert_eq!(
        outcome
            .expect_err("an in-flight rotation must block the CLI codex delete")
            .to_string(),
        "'cx-held' has a token rotation in progress, retry in a moment"
    );
}

/// A quarantined codex profile refuses `clauth start` by name — `--explain`
/// included, the way the claude arm runs `admit` there — naming the fix,
/// before any spawn. The explain leg runs first: it is the one that can red by
/// assertion if the refusal moves or goes (it prints the pick and returns
/// `Ok`), where the real leg would run on into `start::run_codex` and spawn
/// the operator's `codex`, whose exit takes the whole test binary down.
#[test]
fn codex_start_refuses_a_quarantined_chain_by_name() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    std::fs::write(clauth.join("codex-profiles.toml"), "profiles = [\"cx\"]\n")
        .expect("write codex state");
    crate::testutil::write_codex_store(
        "cx",
        &crate::testutil::codex_auth_body(&crate::testutil::jwt_with_exp(1_700_000_060), "rt.a"),
    );
    let expired = |_t: &str| -> Result<
        crate::codex_auth::CodexTokenResponse,
        crate::codex_auth::CodexRefreshError,
    > { Err(crate::codex_auth::CodexRefreshError::Dead("expired")) };
    assert_eq!(
        crate::codex_auth::standby_pass(
            "cx",
            1_700_000_000_000,
            "2026-08-13T00:00:00Z".into(),
            &expired
        ),
        crate::codex_auth::StandbyOutcome::Failed
    );

    for explain_only in [true, false] {
        let err = cmd_start(
            &crate::cli::StartTarget::Named("CX".to_owned()),
            &[],
            Isolation::Shared,
            false,
            explain_only,
        )
        .expect_err("a dead chain refuses the start");
        assert_eq!(
            err.to_string(),
            "'cx': codex chain is broken (expired since 2026-08-13T00:00:00Z), \
             run `clauth login cx --codex --browser`",
            "explain_only = {explain_only}"
        );
    }
}

// ── login paste door: the piped reader and the raw-mode key loop ─────────────

/// The piped-stdin reader behind the paste door: a driver writes one line and
/// may close stdin without a newline; an EOF or a blank line is `Ok(None)`
/// (the browser door keeps waiting), and nothing longer than the cap is held.
#[test]
fn read_manual_code_from_accepts_one_bounded_line() {
    use std::io::Cursor;
    let ok = |s: &str| super::read_manual_code_from(Cursor::new(s.as_bytes().to_vec()));
    let line = |s: &str| ok(s).expect("must read").map(|l| l.trim().to_string());
    assert_eq!(line("abc#st\n").as_deref(), Some("abc#st"));
    assert_eq!(
        line("abc#st").as_deref(),
        Some("abc#st"),
        "a line ended by EOF passes"
    );
    assert_eq!(
        line("abc#st\nsecond line\n").as_deref(),
        Some("abc#st"),
        "only the first line is the code"
    );
    assert!(ok("").expect("immediate eof").is_none(), "EOF is Ok(None)");
    assert!(
        ok("   \n").expect("blank line").is_none(),
        "a blank line is Ok(None)"
    );
    // The cap is judged on the code, not the line: exactly the cap passes
    // with a `\n`, a CRLF, or EOF behind it, and one byte more is refused
    // however the line ends.
    let exact = "a".repeat(crate::oauth_login::MANUAL_CODE_MAX);
    for tail in ["\n", "\r\n", ""] {
        let got = ok(&format!("{exact}{tail}")).unwrap_or_else(|e| panic!("{tail:?}: {e}"));
        assert_eq!(
            got.as_deref().map(|s| s.trim_end_matches(['\r', '\n'])),
            Some(exact.as_str()),
            "{tail:?}"
        );
    }
    let long = "a".repeat(crate::oauth_login::MANUAL_CODE_MAX + 1);
    for tail in ["\n", "\r\n", ""] {
        let err = ok(&format!("{long}{tail}")).expect_err("one over the cap");
        assert_eq!(
            err.to_string(),
            crate::oauth_login::ManualCodeError::TooLong.message(),
            "{tail:?}: the canned message, never the input"
        );
        assert!(!err.to_string().contains("aaaa"), "never echoes the input");
    }
}

/// The raw-mode paste loop's pure step: which keystrokes edit, submit, or
/// cancel, and that a non-`Press` event (Windows delivers release events too)
/// changes nothing.
#[test]
fn feed_paste_key_decides_append_cap_backspace_submit_and_cancel() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    let mut buf = String::new();
    assert!(matches!(
        super::feed_paste_key(&mut buf, crate::testutil::key(KeyCode::Char('a'))),
        super::PasteKey::Continue
    ));
    assert_eq!(buf, "a");
    super::feed_paste_key(&mut buf, crate::testutil::key(KeyCode::Char('#')));
    assert_eq!(buf, "a#");
    assert!(matches!(
        super::feed_paste_key(&mut buf, crate::testutil::key(KeyCode::Backspace)),
        super::PasteKey::Continue
    ));
    assert_eq!(buf, "a");
    assert!(matches!(
        super::feed_paste_key(&mut buf, crate::testutil::key(KeyCode::Enter)),
        super::PasteKey::Submit
    ));
    assert!(matches!(
        super::feed_paste_key(&mut buf, crate::testutil::key(KeyCode::Esc)),
        super::PasteKey::Cancel
    ));
    assert!(matches!(
        super::feed_paste_key(
            &mut buf,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        super::PasteKey::Cancel
    ));
    // A non-Press event is a no-op.
    let before = buf.clone();
    assert!(matches!(
        super::feed_paste_key(
            &mut buf,
            KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            )
        ),
        super::PasteKey::Continue
    ));
    assert_eq!(buf, before, "a Release must not edit the buffer");
    // A Char at the cap is dropped, never echoed: the buffer stays at the cap.
    let mut full = "a".repeat(crate::oauth_login::MANUAL_CODE_MAX);
    assert!(matches!(
        super::feed_paste_key(&mut full, crate::testutil::key(KeyCode::Char('b'))),
        super::PasteKey::Continue
    ));
    assert_eq!(
        full.len(),
        crate::oauth_login::MANUAL_CODE_MAX,
        "cap is exact"
    );
    super::feed_paste_key(&mut full, crate::testutil::key(KeyCode::Backspace));
    assert_eq!(full.len(), crate::oauth_login::MANUAL_CODE_MAX - 1);
}

/// The paste loop's other exit, decided between keystrokes off the progress
/// channel: a landed door ends the prompt, and so does a worker that returned
/// with none (the channel disconnects), or the prompt would outlive the login
/// and its error would print as `login canceled`. A verify bump cannot arrive
/// before the door it follows, and an empty channel keeps the prompt.
#[test]
fn the_paste_prompt_ends_on_a_landed_door_or_a_finished_worker() {
    use crate::oauth_login::{LoginMethod, LoginProgress};
    use std::sync::mpsc::TryRecvError;
    for door in [LoginMethod::Browser, LoginMethod::Manual] {
        assert!(
            super::worker_done(Ok(LoginProgress::ExchangingCode(door))),
            "{door:?}: a landed door ends the prompt"
        );
    }
    assert!(
        super::worker_done(Err(TryRecvError::Disconnected)),
        "a worker that gave up ends the prompt"
    );
    assert!(
        !super::worker_done(Ok(LoginProgress::Verifying)),
        "a verify bump keeps it"
    );
    assert!(
        !super::worker_done(Err(TryRecvError::Empty)),
        "an empty channel keeps it"
    );
}

/// `--manual` is gone: an argv naming it is now an unknown-flag refusal, like
/// any other flag that never existed.
#[test]
fn login_rejects_the_removed_manual_flag() {
    assert_eq!(
        parse_exit_code(&["login", "acme", "--manual"]),
        2,
        "a removed flag must be a usage error, not silently ignored"
    );
}

// ── cmd_start's --auto / --explain wiring ──────────────────────────────────

fn cmd_start_usage() -> crate::usage::UsageInfo {
    crate::usage::UsageInfo {
        five_hour: Some(crate::usage::UsageWindow {
            utilization: 5.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(
                crate::usage::now_epoch_secs() + 3600,
            )),
        }),
        seven_day: Some(crate::usage::UsageWindow {
            utilization: 10.0,
            resets_at: Some(crate::usage::epoch_secs_to_iso(
                crate::usage::now_epoch_secs() + 3600,
            )),
        }),
        fetched_at: Some(crate::usage::now_ms() - 240_000),
        ..Default::default()
    }
}

#[test]
fn cmd_start_explain_launches_nothing() {
    let _sb = crate::testutil::HomeSandbox::new();
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec!["a".into(), "b".into()],
        fallback_chain: vec!["a".into(), "b".into()],
        ..crate::profile::AppState::default()
    })
    .unwrap();
    for name in ["a", "b"] {
        crate::profile_cache::write_profile_cache(
            &crate::profile::ProfileName::from(name),
            crate::profile_cache::USAGE_CACHE_FILE,
            &cmd_start_usage(),
        );
    }

    cmd_start(
        &crate::cli::StartTarget::Auto,
        &[],
        Isolation::Shared,
        false,
        true,
    )
    .unwrap();

    let a_runtime = crate::profile::profile_dir(&crate::profile::ProfileName::from("a"))
        .unwrap()
        .join("runtime");
    let b_runtime = crate::profile::profile_dir(&crate::profile::ProfileName::from("b"))
        .unwrap()
        .join("runtime");
    assert!(
        !a_runtime.exists(),
        "explain must not materialize a runtime for a"
    );
    assert!(
        !b_runtime.exists(),
        "explain must not materialize a runtime for b"
    );
}

#[test]
fn cmd_start_auto_refuses_an_empty_chain() {
    let _sb = crate::testutil::HomeSandbox::new();
    let err = cmd_start(
        &crate::cli::StartTarget::Auto,
        &[],
        Isolation::Shared,
        false,
        false,
    )
    .expect_err("an empty fallback chain must refuse --auto");
    assert_eq!(
        err.to_string(),
        "--auto picks from the fallback chain and it is empty; add accounts on the fallback tab, or name one"
    );
}

#[test]
fn cmd_start_explain_auto_runs_the_with_fallback_refusals() {
    let _sb = crate::testutil::HomeSandbox::new();
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec!["a".into()],
        fallback_chain: vec!["a".into()],
        ..crate::profile::AppState::default()
    })
    .unwrap();
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("a"),
        crate::profile_cache::USAGE_CACHE_FILE,
        &cmd_start_usage(),
    );

    let err = cmd_start(
        &crate::cli::StartTarget::Auto,
        &[],
        Isolation::Shared,
        true,
        true,
    )
    .expect_err("--with-fallback under --auto --explain must run the chain refusals");
    assert_eq!(
        err.to_string(),
        "'a': --with-fallback needs a second account in the fallback chain to move to; add one on the fallback tab, or start without it"
    );
}

// ── the claude-only verbs and a codex name ────────────────────────────────────

/// `disable`/`enable`/`rolling-token`/`static-token` take claude names alone.
/// A codex name is refused as what it is, a real account on the other harness,
/// in one fixed shape naming the verb, as a usage error (exit 2); a name on
/// neither roster lists the claude roster alone, the only one these verbs
/// take. `switch`/`delete`/`start` keep the two-roster listing.
#[test]
fn the_claude_only_verbs_refuse_a_codex_name_and_list_the_claude_roster_alone() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    std::fs::write(clauth.join("codex-profiles.toml"), "profiles = [\"cx\"]\n")
        .expect("write codex state");
    let config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["cl1".into()],
            ..Default::default()
        },
        profiles: vec![crate::testutil::blank_profile(&ProfileName::from("cl1"))],
    };

    let err = resolve_or_bail(&config, "cx", "disable").expect_err("a codex name is refused");
    assert!(
        err.downcast_ref::<UsageError>().is_some(),
        "a usage error, so the process exits 2: {err:?}"
    );
    assert_eq!(
        err.to_string(),
        "'cx' is a codex profile; disable is claude-only"
    );
    // The caller's casing resolves to the roster's spelling, as `switch` does.
    let err = resolve_or_bail(&config, "CX", "enable").expect_err("a codex name is refused");
    assert_eq!(
        err.to_string(),
        "'cx' is a codex profile; enable is claude-only"
    );

    let err = resolve_or_bail(&config, "zz", "disable").expect_err("unknown on both rosters");
    assert!(err.downcast_ref::<UsageError>().is_some(), "{err:?}");
    assert_eq!(err.to_string(), "profile 'zz' not found\navailable: cl1");

    assert_eq!(
        unknown_profile_error(&config, "zz").to_string(),
        "profile 'zz' not found\navailable: cl1 · codex: cx",
        "control: the two-roster listing `switch`/`delete`/`start` use is unchanged"
    );

    let claude = resolve_or_bail(&config, "CL1", "disable").expect("a claude name resolves");
    assert_eq!(claude.as_str(), "cl1");
}

/// Every handler passes its own verb, so `clauth <verb> cx` names the verb the
/// user typed; the `--clear` form of `static-token` is the same verb. Through
/// `dispatch` the refusal maps to exit 2.
#[test]
fn each_claude_only_verb_names_itself_in_the_codex_refusal() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    std::fs::write(clauth.join("codex-profiles.toml"), "profiles = [\"cx\"]\n")
        .expect("write codex state");

    let cases = [
        ("disable", cmd_disable("cx", true)),
        ("enable", cmd_enable("cx")),
        ("rolling-token", cmd_rolling_token("cx")),
        ("static-token", cmd_static_token("cx")),
        ("static-token", cmd_static_token_clear("cx", true)),
    ];
    for (verb, outcome) in cases {
        let err = outcome.expect_err("a codex name is refused before any state moves");
        assert!(
            err.downcast_ref::<UsageError>().is_some(),
            "{verb}: {err:?}"
        );
        assert_eq!(
            err.to_string(),
            format!("'cx' is a codex profile; {verb} is claude-only")
        );
    }

    let cli = parse(&["disable", "cx", "--yes"]).expect("argv parses");
    assert_eq!(crate::exit_code(crate::dispatch(cli)), 2);
}

/// A roster that fails to load is a runtime failure (exit 1) like `switch`'s,
/// `delete`'s and `start`'s, never a not-found: without the roster the verdict
/// on `cx` is unknowable, and `profile 'cx' not found` would send the user to
/// fix the wrong file. A claude name never loads the roster, so it resolves
/// over the same corrupt file.
#[test]
fn a_corrupt_codex_roster_fails_the_claude_only_verbs_as_a_runtime_error() {
    let _home = crate::testutil::HomeSandbox::new();
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    crate::profile::mkdir_700(&clauth).expect("mkdir .clauth");
    let roster = clauth.join("codex-profiles.toml");
    std::fs::write(&roster, "profiles = [not toml").expect("corrupt codex state");
    let config = AppConfig {
        state: crate::profile::AppState {
            profiles: vec!["cl1".into()],
            ..Default::default()
        },
        profiles: vec![crate::testutil::blank_profile(&ProfileName::from("cl1"))],
    };

    let err = resolve_or_bail(&config, "cx", "disable").expect_err("the roster does not load");
    assert_eq!(
        err.to_string(),
        format!("failed to parse {}", roster.display())
    );
    assert!(
        err.root_cause().downcast_ref::<toml::de::Error>().is_some(),
        "the parse error is the cause, never hidden: {err:?}"
    );
    assert!(
        err.downcast_ref::<UsageError>().is_none(),
        "a runtime failure, not a usage error: {err:?}"
    );
    assert_eq!(crate::exit_code(Err(err)), 1);

    let claude =
        resolve_or_bail(&config, "CL1", "disable").expect("a claude name never loads the roster");
    assert_eq!(claude.as_str(), "cl1");
}

// ── use-reset ─────────────────────────────────────────────────────────────────

#[test]
fn use_reset_takes_list_or_yes_but_not_both() {
    let Command::UseReset { profile, list, yes } = command(&["use-reset", "cx"]) else {
        panic!("must parse");
    };
    assert_eq!((profile.as_str(), list, yes), ("cx", false, false));
    let Command::UseReset { list, yes, .. } = command(&["use-reset", "-y", "cx"]) else {
        panic!("must parse");
    };
    assert_eq!((list, yes), (false, true));
    let Command::UseReset { list, yes, .. } = command(&["use-reset", "cx", "--list"]) else {
        panic!("must parse");
    };
    assert_eq!((list, yes), (true, false));

    assert_eq!(
        parse_exit_code(&["use-reset", "cx", "--list", "--yes"]),
        2,
        "--list spends nothing, so there is nothing for --yes to confirm"
    );
    assert_eq!(parse_exit_code(&["use-reset"]), 2);
    assert_eq!(parse_exit_code(&["use-reset", "cx", "other"]), 2);
}

/// A claude profile `cl1`, a codex profile `cx` with a stored login, and a
/// codex profile `cy` with none.
fn seed_use_reset_rosters() {
    crate::profile::save_app_state(&crate::profile::AppState {
        profiles: vec!["cl1".into()],
        ..crate::profile::AppState::default()
    })
    .expect("claude state");
    let clauth = crate::profile::clauth_dir().expect("clauth dir");
    std::fs::write(
        clauth.join("codex-profiles.toml"),
        "profiles = [\"cx\", \"cy\"]\n",
    )
    .expect("codex state");
    crate::testutil::write_codex_store("cx", &crate::testutil::codex_auth_body("at.cx", "rt.cx"));
}

/// Endpoints on a listener that is bound but never accepted: a request would
/// queue a connection on it, so `accept` answering `WouldBlock` afterwards
/// proves nothing was sent — at once, with no stub deadline to wait out.
fn untouched_reset_urls() -> (std::net::TcpListener, usage::codex_reset::ResetUrls) {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let urls = usage::codex_reset::ResetUrls::under(&format!("http://127.0.0.1:{port}"));
    (listener, urls)
}

fn assert_nothing_sent(listener: &std::net::TcpListener) {
    match listener.accept() {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("a request reached the endpoint: {other:?}"),
    }
}

fn no_prompt(prompt: &str) -> Result<bool> {
    panic!("this path must not prompt, asked: {prompt}")
}

/// `use-reset` takes codex names alone: a claude name is refused as one (the
/// codex-side twin of the claude-only verbs' refusal), an unknown name lists
/// both rosters, and a codex name with no stored login or a dead chain is
/// refused by name — all before any request.
#[test]
fn use_reset_refuses_a_claude_name_an_unknown_one_and_a_dead_chain_before_any_request() {
    let _home = crate::testutil::HomeSandbox::new();
    seed_use_reset_rosters();
    let (listener, urls) = untouched_reset_urls();

    let err = use_reset_with("CL1", false, true, false, &urls, no_prompt).expect_err("claude");
    assert!(err.downcast_ref::<UsageError>().is_some(), "{err:?}");
    assert_eq!(
        err.to_string(),
        "'cl1' is a claude profile; use-reset is codex-only"
    );

    let err = use_reset_with("zz", false, true, false, &urls, no_prompt).expect_err("unknown");
    assert!(err.downcast_ref::<UsageError>().is_some(), "{err:?}");
    assert_eq!(
        err.to_string(),
        "profile 'zz' not found\navailable: cl1 · codex: cx, cy"
    );

    let err = use_reset_with("cy", false, true, false, &urls, no_prompt).expect_err("no login");
    assert_eq!(
        err.to_string(),
        "'cy' has no stored codex login to use a reset with; run `clauth login cy --codex --browser`"
    );

    crate::codex_auth::quarantine_for_test("cx", "reused", "rt.cx");
    let err = use_reset_with("CX", true, false, false, &urls, no_prompt).expect_err("dead chain");
    assert!(
        err.to_string()
            .starts_with("'cx': codex chain is broken (reused since "),
        "{err}"
    );
    crate::codex_auth::clear_quarantine("cx");

    assert_nothing_sent(&listener);
}

/// Off a terminal the spend needs `--yes`, and the refusal lands before any
/// request leaves: the delete/disable confirm policy, for a spend that cannot
/// be given back. Exit 1, like theirs.
#[test]
fn use_reset_off_a_terminal_without_yes_refuses_before_any_request() {
    let _home = crate::testutil::HomeSandbox::new();
    seed_use_reset_rosters();
    let (listener, urls) = untouched_reset_urls();

    let err = use_reset_with("cx", false, false, false, &urls, no_prompt).expect_err("refused");
    assert_eq!(
        err.to_string(),
        "refusing to use a reset on 'cx' without confirmation; pass --yes"
    );
    assert_nothing_sent(&listener);
    assert_eq!(crate::exit_code(Err(err)), 1);
}

/// End to end against a stub: `--list` only lists (and needs no terminal), a
/// declined prompt sends nothing, a confirmed one consumes the credit the
/// prompt named — the one expiring first — under a fresh v4 key, and an
/// account with nothing available fails without a consume. The request order
/// is the proof: exactly one POST, right after the confirmed GET.
#[test]
fn use_reset_spends_the_credit_it_named_and_only_after_a_yes() {
    let _home = crate::testutil::HomeSandbox::new();
    seed_use_reset_rosters();
    let two = r#"{"credits": [
        {"id": "later", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-01T00:00:00Z", "expires_at": "2027-06-01T00:00:00Z"},
        {"id": "soon", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-02T00:00:00Z", "expires_at": "2027-01-01T00:00:00Z", "title": "Full reset"}
    ], "available_count": 2}"#;
    let none = r#"{"credits": [{"id": "soon", "reset_type": "codex_rate_limits", "status": "redeemed", "granted_at": "2026-01-02T00:00:00Z"}], "available_count": 0}"#;
    let (addr, handle) = crate::testutil::serve_endpoints_raw(5, move |_path, i| match i {
        3 => (200, r#"{"code": "reset", "windows_reset": 2}"#.to_string()),
        4 => (200, none.to_string()),
        _ => (200, two.to_string()),
    });
    let urls = usage::codex_reset::ResetUrls::under(&addr);

    use_reset_with("cx", true, false, false, &urls, no_prompt).expect("--list lists");

    let mut asked = String::new();
    use_reset_with("cx", false, false, true, &urls, |prompt| {
        asked = prompt.to_string();
        Ok(false)
    })
    .expect("a declined prompt is a clean abort");
    assert!(
        asked.contains("'cx'? Full reset · expires ") && asked.contains("1 of 2 available"),
        "the prompt names the credit it will spend: {asked}"
    );

    use_reset_with("cx", false, false, true, &urls, |_| Ok(true)).expect("confirmed spend");

    let err = use_reset_with("cx", false, true, false, &urls, no_prompt).expect_err("none left");
    assert_eq!(err.to_string(), "no usage-limit resets available on 'cx'");

    let seen = handle.join().expect("join stub");
    let lines: Vec<&str> = seen
        .iter()
        .map(|r| r.lines().next().unwrap_or(""))
        .collect();
    let list = "GET /backend-api/wham/rate-limit-reset-credits HTTP/1.1";
    let consume = "POST /backend-api/wham/rate-limit-reset-credits/consume HTTP/1.1";
    assert_eq!(lines, [list, list, list, consume, list]);
    let body: serde_json::Value =
        serde_json::from_str(&crate::testutil::request_body(&seen[3])).expect("json body");
    assert_eq!(body["credit_id"], "soon", "the earliest-expiring credit");
    let key = body["redeem_request_id"].as_str().expect("a key");
    assert_eq!(
        (key.len(), key.chars().nth(14)),
        (36, Some('4')),
        "a v4 UUID: {key}"
    );
    assert_eq!(
        crate::testutil::request_header(&seen[3], "chatgpt-account-id").as_deref(),
        Some("acc"),
        "the store's account id rides along"
    );
}
