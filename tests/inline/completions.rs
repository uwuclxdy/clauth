//! Shell-completions feature coverage: the advertised
//! `clauth completions install [shell]` path. `print_script` is a pure
//! shell→script lookup; `install_rc` / `install_fish` write into home-derived
//! paths, so they run under a home sandbox.

use super::*;

#[test]
fn print_script_supports_bash_zsh_fish() {
    for shell in ["bash", "zsh", "fish"] {
        print_script(shell).unwrap_or_else(|_| panic!("{shell} must be supported"));
    }
}

/// Every shell's script must offer `--isolated` under the `start` subcommand —
/// it's a documented `clauth start` flag (`main.rs`) and was previously uncovered.
#[test]
fn every_shell_completes_start_isolated_flag() {
    // (script body, the flag token as each shell spells its `start` branch)
    let cases = [
        (
            &BASH,
            "\"${COMP_WORDS[1]}\" = \"start\" ] && [ \"${cur:0:2}\" = \"--\"",
        ),
        (
            &ZSH,
            "\"${words[2]}\" == start ]] && _values 'flag' '--isolated",
        ),
        (&FISH, "__fish_seen_subcommand_from start\" -a --isolated"),
    ];
    for (script, branch) in cases {
        assert!(
            script.contains("--isolated"),
            "script must offer --isolated",
        );
        assert!(
            script.contains(branch),
            "the --isolated completion must be gated to the `start` subcommand, not global",
        );
    }
    // Guard against regressing the other subcommands' flags in the same edit.
    assert!(ZSH.contains("--json") && ZSH.contains("--base-url") && ZSH.contains("--force"));
}

/// Every shell's script must offer `--with-fallback` under `start`, gated to
/// that subcommand. The tree walk below scopes each flag to its subcommand's
/// branch; what it does not pin is the exact spelling and ordering inside that
/// branch, which is what these needles hold.
#[test]
fn every_shell_completes_start_with_fallback_flag() {
    let cases = [
        (&BASH, "--isolated --with-fallback"),
        // Anchored on the preceding sibling INSIDE the backslash-continued
        // block. zsh's describe entry is position-free on its own, so a needle
        // made only of it stays green while the line is moved under another
        // subcommand's branch and the flag stops being offered under `start`.
        (
            &ZSH,
            "'--isolated[clean isolated runtime; drops operator config]' \\\n            \
             '--with-fallback[follow the fallback chain",
        ),
        (
            &FISH,
            "__fish_seen_subcommand_from start\" -a --with-fallback",
        ),
    ];
    for (script, gated) in cases {
        assert!(
            script.contains("--with-fallback"),
            "script must offer --with-fallback",
        );
        assert!(
            script.contains(gated),
            "the --with-fallback completion must be gated to `start`, missing {gated:?}",
        );
    }
}

/// The flag pair that used to decide the isolated rescue is gone from the
/// grammar, so a script still offering it completes a spelling clap refuses —
/// the one shell surface where a removal is silent, since nothing here parses.
/// Whole-word, so a hit is a completion entry rather than these letters sitting
/// inside some longer token — the two spellings cannot hide inside each other,
/// which is a property of this pair and not of the check.
#[test]
fn no_shell_offers_the_removed_rescue_flags() {
    for (shell, script) in [("bash", &BASH), ("zsh", &ZSH), ("fish", &FISH)] {
        for flag in ["--rescue", "--no-rescue"] {
            assert!(
                !offers_token(script, flag),
                "{shell} must not complete the removed {flag}",
            );
        }
    }
}

/// `clauth start --with-fallback <TAB>` is the canonical shape — clap only sees
/// the flag before the profile name — so the profile list has to follow it in the
/// two position-sensitive shells. fish matches on the subcommand alone and is
/// unaffected.
#[test]
fn bash_and_zsh_complete_a_profile_after_start_with_fallback() {
    assert!(
        BASH.contains(
            r#"[ "$prev" = "--isolated" ] || [ "$prev" = "--with-fallback" ] || [ "$prev" = "--profile" ]"#
        ),
        "bash must list profiles after --with-fallback, not only after --isolated",
    );
    assert!(
        ZSH.contains(r#""${words[2]}" == start && "${words[3]}" == (--isolated|--with-fallback)"#),
        "zsh's fourth-word profile arm must accept --with-fallback as the third word",
    );
}

/// Every shell must offer `--setup-token` under the `login` subcommand — the
/// long-lived-token capture flow (#53), gated to login like the other login
/// flags. Mirrors the `--isolated` coverage above.
#[test]
fn every_shell_completes_login_setup_token_flag() {
    let cases = [
        (&BASH, "--base-url --api-key --setup-token"),
        (&ZSH, "'--setup-token[capture a claude setup-token"),
        (
            &FISH,
            "__fish_seen_subcommand_from login\" -a --setup-token",
        ),
    ];
    for (script, gated) in cases {
        assert!(
            script.contains("--setup-token"),
            "script must offer --setup-token",
        );
        assert!(
            script.contains(gated),
            "the --setup-token completion must be gated to `login`, missing {gated:?}",
        );
    }
}

/// The scripts no longer spell the login flags themselves — the built statics
/// splice `crate::cli::LOGIN_FLAGS` over the marker. A script still carrying
/// it got an empty splice: every login completion missing, which the grammar
/// walk below also reds, but one report per flag.
#[test]
fn no_script_carries_the_login_flags_marker() {
    for (shell, script) in [("bash", &BASH), ("zsh", &ZSH), ("fish", &FISH)] {
        assert!(
            !script.contains(LOGIN_FLAGS_MARKER),
            "{shell} still carries the raw {LOGIN_FLAGS_MARKER} marker",
        );
    }
}

/// The splice must drop no entry: every `LOGIN_FLAGS` flag is offered by every
/// dialect. (That it is gated to `login` is the grammar walk's job below.)
#[test]
fn every_login_flag_is_offered_by_all_three_scripts() {
    for (shell, script) in [("bash", &BASH), ("zsh", &ZSH), ("fish", &FISH)] {
        for flag in crate::cli::LOGIN_FLAGS {
            assert!(script.contains(flag), "{shell} must offer {flag}");
        }
    }
}

/// `LOGIN_FLAGS` is spliced into every script, so a stale entry would complete
/// a spelling clap refuses — the rescue-flags class, on a list only this test
/// reads. The grammar walk pins the other direction (a new clap flag missing
/// from the scripts); this pins the list to clap's own `login` args.
#[test]
fn login_flags_matches_claps_login_args() {
    use clap::CommandFactory as _;

    let command = crate::cli::Cli::command();
    let login = command.find_subcommand("login").expect("login subcommand");
    // Both spellings of one clap arg (`--yes` and `-y`) are separate
    // completion entries, so collect long and short each rather than one
    // spelling per argument.
    let mut clap_flags: Vec<String> = login
        .get_arguments()
        .flat_map(|a| {
            a.get_long()
                .map(|l| format!("--{l}"))
                .into_iter()
                .chain(a.get_short().map(|s| format!("-{s}")))
        })
        .collect();
    clap_flags.sort_unstable();
    let mut ours: Vec<String> = crate::cli::LOGIN_FLAGS
        .iter()
        .map(|f| (*f).to_string())
        .collect();
    ours.sort_unstable();
    assert_eq!(
        ours, clap_flags,
        "LOGIN_FLAGS must be exactly clap's login flags"
    );
}

/// `clauth herdr install` and `clauth herdr uninstall` are the only herdr
/// subcommands whose flags the scripts offer, so the flag branches must track
/// them: after `clauth herdr config get <key>` clap refuses `--key
/// --no-config --yes`, and the scripts must not complete what clap rejects.
#[test]
fn herdr_flags_are_offered_only_under_install_and_uninstall() {
    let cases = [
        (
            &BASH,
            r#""${COMP_WORDS[1]}" = "herdr" ] && [ "${COMP_WORDS[2]}" = "install" ]"#,
        ),
        (
            &ZSH,
            r#""${words[2]}" == herdr && "${words[3]}" == install"#,
        ),
        (
            &FISH,
            "__fish_seen_subcommand_from herdr; and __fish_seen_subcommand_from install\" -a --key",
        ),
    ];
    for (script, branch) in cases {
        for flag in ["--key", "--no-config", "--yes"] {
            assert!(script.contains(flag), "script must offer {flag}");
        }
        assert!(
            script.contains(branch),
            "the install flag offer must be gated to `install`, missing {branch:?}",
        );
    }
    // `uninstall` offers the same flags minus `--key`, and each shell gates
    // that arm on the subcommand too.
    for (script, branch) in [
        (
            &BASH,
            r#""${COMP_WORDS[1]}" = "herdr" ] && [ "${COMP_WORDS[2]}" = "uninstall" ]"#,
        ),
        (
            &ZSH,
            r#""${words[2]}" == herdr && "${words[3]}" == uninstall"#,
        ),
        (
            &FISH,
            "__fish_seen_subcommand_from herdr; and __fish_seen_subcommand_from uninstall\" -a --no-config",
        ),
    ] {
        assert!(
            script.contains(branch),
            "the uninstall flag offer must be gated to `uninstall`, missing {branch:?}",
        );
    }
    // The arm that follows `config get` completes knob names, never the
    // install flags above.
    assert!(
        BASH.contains(
            r#"compgen -W "popup_width pane_tag tag_watch_secs border_label delegate_dot delegate_row_text""#
        ),
        "bash's `config get` arm offers the knobs and nothing else"
    );
    // The subcommand offer itself stays gated on `herdr` alone: the flag
    // offer below it is the only one the `install` clause may gate.
    assert!(
        FISH.contains(r#"-n "__fish_seen_subcommand_from herdr" -a install"#),
        "fish must still offer `install` right after `clauth herdr`"
    );
    assert!(
        FISH.contains(r#"-n "__fish_seen_subcommand_from herdr" -a uninstall"#),
        "fish must still offer `uninstall` right after `clauth herdr`"
    );
    assert!(
        FISH.contains(r#"-n "__fish_seen_subcommand_from herdr" -a config"#),
        "fish must still offer `config` right after `clauth herdr`"
    );
}

/// The scripts are hand-written (clap_complete's stable generator can't
/// reproduce the live `clauth __complete` profile-name shellout), so nothing
/// structural keeps them level with the grammar — they had already drifted three
/// subcommands and a root flag behind it. This walks the real clap `Command`
/// tree and fails on the next drift instead of waiting for someone to notice.
///
/// Each flag is looked up inside its own subcommand's completion branch, not
/// anywhere in the script: spellings repeat across subcommands (`--all` under
/// both `status` and `list`), so a whole-script match would let one of them be
/// deleted while a sibling kept the token alive.
///
/// `help` and `version` are excluded: clap generates them for every command and
/// no shell needs them completed.
#[test]
fn every_visible_subcommand_and_long_flag_is_offered_by_all_three_scripts() {
    use clap::CommandFactory as _;

    let root = crate::cli::Cli::command();
    let generated = ["help", "version"];

    let mut expected: Vec<(String, String)> = root
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter_map(|a| a.get_long())
        .filter(|l| !generated.contains(l))
        .map(|l| ("<root>".to_string(), format!("--{l}")))
        .collect();

    for sub in root.get_subcommands().filter(|s| !s.is_hide_set()) {
        let name = sub.get_name().to_string();
        if generated.contains(&name.as_str()) {
            continue;
        }
        expected.push((name.clone(), name.clone()));
        for long in sub
            .get_arguments()
            .filter(|a| !a.is_hide_set())
            .filter_map(|a| a.get_long())
            .filter(|l| !generated.contains(l))
        {
            expected.push((name.clone(), format!("--{long}")));
        }
    }

    assert!(
        expected.len() > 20,
        "the walk found only {} tokens — it stopped seeing the grammar, \
         so a green run would prove nothing",
        expected.len()
    );
    // The owner half is the dimension the scoping below rests on: a walk that
    // collapsed every pair onto `<root>` would still clear the count guard.
    let owners = expected
        .iter()
        .map(|(owner, _)| owner.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        owners.len() > 10,
        "the walk attributed tokens to only {} owners — it stopped seeing the \
         subcommands, so the per-subcommand scoping below would prove nothing",
        owners.len()
    );

    let mut missing: Vec<String> = Vec::new();
    for (shell, script) in [("bash", &BASH), ("zsh", &ZSH), ("fish", &FISH)] {
        for (owner, token) in &expected {
            // A subcommand's own name is offered by the first-word branch; only
            // its flags live under the branch named after it.
            let branch = if owner == "<root>" || owner == token {
                root_branch(shell, script)
            } else {
                subcommand_branch(shell, script, owner)
            };
            match branch {
                // A bare `contains` would let `--standby` pass on `--no-standby`
                // alone, so match the token with no `-`/alphanumeric neighbour.
                Some(branch) if offers_token(&branch, token) => {}
                Some(_) => missing.push(format!("{shell}: {owner} → {token}")),
                None => missing.push(format!(
                    "{shell}: {owner} → {token} (no `{owner}` branch in the script)"
                )),
            }
        }
    }
    assert!(
        missing.is_empty(),
        "completion scripts have drifted from the clap grammar:\n  {}",
        missing.join("\n  ")
    );
}

/// The slice of `script` that completes the word right after `clauth`: the
/// subcommand names and the root's own flags.
fn root_branch(shell: &str, script: &str) -> Option<String> {
    match shell {
        "bash" => guarded_arms(script, |guard| guard.contains("\"$COMP_CWORD\" -eq 1")),
        "zsh" => guarded_arms(script, |guard| guard.contains("(( CURRENT == 2 ))")),
        "fish" => joined(
            script
                .lines()
                .filter(|l| l.contains("__fish_is_first_token")),
        ),
        _ => None,
    }
}

/// The slice of `script` that completes `name`'s own flags. `None` means the
/// script has no such branch at all — the caller reports that as drift, because
/// falling back to a whole-script search is exactly the hole the scoping closes.
fn subcommand_branch(shell: &str, script: &str, name: &str) -> Option<String> {
    match shell {
        // bash pins the subcommand either as the first word (the flag arms) or
        // as the word just typed (the arms offering a profile after it).
        "bash" => guarded_arms(script, |guard| {
            guard.contains(&format!("\"${{COMP_WORDS[1]}}\" = \"{name}\""))
                || guard.contains(&format!("\"$prev\" = \"{name}\""))
        }),
        // zsh pins it in `[[ "${words[2]}" == … ]]`, bare or as an alternation.
        "zsh" => guarded_arms(script, |guard| {
            guard
                .split("\"${words[2]}\" == ")
                .skip(1)
                .filter_map(|rest| rest.split_whitespace().next())
                .any(|pat| pat.trim_matches(['(', ')']).split('|').any(|a| a == name))
        }),
        // fish pins it in a `__fish_seen_subcommand_from` condition, which may
        // name several subcommands.
        "fish" => joined(script.lines().filter(|l| {
            l.split("__fish_seen_subcommand_from ")
                .skip(1)
                .filter_map(|rest| rest.split('"').next())
                .any(|list| list.split_whitespace().any(|w| w == name))
        })),
        _ => None,
    }
}

/// Every arm of the script's `if`/`elif` chain whose guard line satisfies
/// `owns`, joined. Both the bash and the zsh script are one such chain, and a
/// subcommand can hold more than one arm (zsh spells `resume` in two). An arm
/// is closed at `else`/`fi` as well as opened at `if`/`elif`: without that, the
/// catch-all body and everything trailing the chain inherit the last `elif`'s
/// owner, which is the "right token, wrong place" pass this scoping exists to
/// stop. A closed arm leads with `else`/`fi`, which no `owns` matches, so it is
/// unowned with no extra filtering.
fn guarded_arms(script: &str, owns: impl Fn(&str) -> bool) -> Option<String> {
    let mut arms: Vec<String> = Vec::new();
    let mut arm = String::new();
    for line in script.lines() {
        let head = line.trim_start();
        let opens = head.starts_with("if ") || head.starts_with("elif ");
        let closes = head.starts_with("else") || head == "fi";
        if (opens || closes) && !arm.is_empty() {
            arms.push(std::mem::take(&mut arm));
        }
        arm.push_str(line);
        arm.push('\n');
    }
    arms.push(arm);
    joined(
        arms.iter()
            .filter(|a| owns(a.lines().next().unwrap_or("")))
            .map(String::as_str),
    )
}

fn joined<'a>(parts: impl Iterator<Item = &'a str>) -> Option<String> {
    let text = parts.collect::<Vec<_>>().join("\n");
    (!text.is_empty()).then_some(text)
}

/// An `else` body, and anything trailing the chain, belong to no subcommand.
/// Without closing the arm there they inherit the last `elif`'s owner, so a flag
/// moved into the catch-all still reads as offered under `list`. The real
/// scripts have neither shape, so only a fixture can hold this.
#[test]
fn a_branch_stops_at_the_catch_all_and_at_the_end_of_the_chain() {
    let script = r#"if [ "${COMP_WORDS[1]}" = "status" ]; then
    COMPREPLY=--json
elif [ "${COMP_WORDS[1]}" = "list" ]; then
    COMPREPLY=--all
else
    COMPREPLY=--catch-all
fi
trailing --after-chain
"#;
    let list = subcommand_branch("bash", script, "list").expect("list branch");
    assert!(offers_token(&list, "--all"), "its own arm is in");
    assert!(
        !offers_token(&list, "--catch-all"),
        "the `else` body is nobody's branch",
    );
    assert!(
        !offers_token(&list, "--after-chain"),
        "text past `fi` is nobody's branch",
    );
    assert!(
        !offers_token(&list, "--json"),
        "the sibling arm above stays out",
    );
}

/// The scoping's two load-bearing properties: a branch is a slice of the script
/// and not the whole of it, and an absent branch is `None` rather than a
/// whole-script fallback.
#[test]
fn subcommand_branch_isolates_one_subcommand_or_reports_none() {
    for (shell, script) in [("bash", &BASH), ("zsh", &ZSH), ("fish", &FISH)] {
        let list = subcommand_branch(shell, script, "list")
            .unwrap_or_else(|| panic!("{shell} must have a `list` branch"));
        assert!(offers_token(&list, "--all"), "{shell}: list offers --all");
        assert!(
            !offers_token(&list, "--json"),
            "{shell}: `list` takes no --json, so its branch must not span the \
             sibling branches that do",
        );
        assert!(
            subcommand_branch(shell, script, "nonesuch").is_none(),
            "{shell}: an absent branch must report itself, not fall back",
        );
    }
}

/// Whether `script` offers `token` as a whole word: a hyphen, letter, digit or
/// underscore on either side disqualifies the hit, so `standby` does not match
/// inside `--no-standby` nor `--standby` inside `--standby-mode`, while `start`
/// still matches inside `-W "start login"`.
fn offers_token(script: &str, token: &str) -> bool {
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_');
    script.match_indices(token).any(|(i, _)| {
        let before = script[..i].chars().next_back().is_none_or(boundary);
        let after = script[i + token.len()..]
            .chars()
            .next()
            .is_none_or(boundary);
        before && after
    })
}

/// Both boundary sides, each fed an input the predicate is what rejects.
///
/// The two flag-pair cases below them are kept for the shapes they document, but
/// neither can fail on its own: `--no-standby` does not CONTAIN `--standby` (one
/// hyphen, not two) and `'--setup-token[x]'` does not contain `start`, so a
/// plain `str::contains` answers them identically. Measured — reducing
/// `boundary` to `|_| true` left the whole gate green until the first two lines
/// existed.
#[test]
fn offers_token_does_not_match_inside_a_longer_flag() {
    // `standby` IS inside `--no-standby`, preceded by a hyphen: the left
    // boundary is the only thing that can reject it.
    assert!(!offers_token("a --no-standby b", "standby"));
    // `--standby` IS inside `--standby-mode`, followed by a hyphen: likewise on
    // the right, which no other case here exercises.
    assert!(!offers_token("a --standby-mode b", "--standby"));
    assert!(offers_token("a --standby b", "--standby"));
    assert!(!offers_token("a --no-standby b", "--standby"));
    assert!(!offers_token("'--setup-token[x]'", "start"));
    assert!(offers_token("-W \"start login\"", "start"));
}

#[test]
fn print_script_rejects_unsupported_shell() {
    let err = print_script("powershell").expect_err("unsupported shell must error");
    assert!(
        err.to_string().contains("unsupported shell"),
        "error must name the unsupported shell",
    );
}

#[cfg(unix)]
use crate::testutil::HomeSandbox;

/// `completions install bash` writes the script under `~/.clauth/completions/`
/// and appends an idempotent `source` line to `~/.bashrc`.
#[cfg(unix)]
#[test]
fn install_bash_writes_script_and_sources_it_in_rc() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("bash")).expect("install bash completions");

    let script = home_path
        .join(".clauth")
        .join("completions")
        .join("clauth.bash");
    assert!(
        script.is_file(),
        "the bash completion script must be written"
    );
    assert!(
        std::fs::read_to_string(&script)
            .expect("read script")
            .contains("complete -F _clauth clauth"),
        "the written script must be the bash completion body",
    );

    let rc = std::fs::read_to_string(home_path.join(".bashrc")).expect("read .bashrc");
    assert!(
        rc.contains(&format!("source \"{}\"", script.display())),
        ".bashrc must source the generated completion script",
    );
}

/// Re-running `install` must not append a second `source` line — the rc edit is
/// idempotent (guarded by the existing-line check).
#[cfg(unix)]
#[test]
fn install_bash_is_idempotent_across_reruns() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("bash")).expect("first install");
    install(Some("bash")).expect("second install");

    let rc = std::fs::read_to_string(home_path.join(".bashrc")).expect("read .bashrc");
    let count = rc.matches("# clauth completions").count();
    assert_eq!(count, 1, "the rc source block must be written exactly once");
}

/// Fish does not edit an rc file: the script lands in fish's own completions dir.
#[cfg(unix)]
#[test]
fn install_fish_writes_into_fish_completions_dir() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("fish")).expect("install fish completions");

    let script = home_path
        .join(".config")
        .join("fish")
        .join("completions")
        .join("clauth.fish");
    assert!(
        script.is_file(),
        "fish completions must be written to the fish completions dir",
    );
    assert!(
        !home_path.join(".bashrc").exists() && !home_path.join(".zshrc").exists(),
        "installing fish must not touch bash/zsh rc files",
    );
}

#[test]
fn install_rejects_unsupported_shell() {
    let err = install(Some("powershell")).expect_err("unsupported shell must error");
    assert!(
        err.to_string().contains("unsupported shell"),
        "error must name the unsupported shell",
    );
}

// The first-launch consent prompt defaults to Yes: an empty answer (bare Enter)
// installs, so the convenient path stays a single keypress.
#[test]
fn answer_is_yes_defaults_to_yes_on_empty() {
    for a in ["", "   ", "\n", "\r\n"] {
        assert!(answer_is_yes(a), "{a:?} (default) must install");
    }
}

#[test]
fn answer_is_yes_accepts_y_and_yes_any_case() {
    for a in ["y", "Y", "yes", "YES", " Yes "] {
        assert!(answer_is_yes(a), "{a:?} must install");
    }
}

#[test]
fn answer_is_yes_declines_on_n_or_other_input() {
    for a in ["n", "N", "no", "nope", "q", "x"] {
        assert!(!answer_is_yes(a), "{a:?} must decline");
    }
}

// ── the scripts must parse in the shells that source them ────────────────────
//
// The grammar walk above compares clap's command tree against the script TEXT
// with `str::contains`. It models no shell lexical state at all, so a quoting
// break is invisible to it — and worse, it DEMANDS the offending line be
// present, so the assertion that should have caught the CLA-ROLL apostrophe
// (`'feed[feed a profile's …]'`, which terminated the zsh single-quoted spec
// and left the whole `_clauth` function unparseable) is the one that certified
// it. `clauth completions install zsh` writes that file and sources it from the
// user's rc, so the blast radius was completion for the ENTIRE `clauth`
// command, plus a parse error on every new shell.
//
// The only thing that can see that class is the shell itself.

/// Run one dialect's own parser over a script. `Ok(None)` = that shell is not
/// installed here.
#[cfg(unix)]
fn parse_check(bin: &str, args: &[&str], script: &str, ext: &str) -> Option<std::process::Output> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("clauth{ext}"));
    std::fs::write(&path, script).expect("write script");
    match std::process::Command::new(bin)
        .args(args)
        .arg(&path)
        .output()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("could not run `{bin}`: {e}"),
        Ok(out) => Some(out),
    }
}

/// The parser reports where it ran out of input, not where the quote opened —
/// on the real regression it said line 65 (the closing `}`) for a fault on
/// line 13. So the message does the localizing the parser refuses to.
#[cfg(unix)]
fn lint_failure(shell: &str, bin: &str, script: &str, out: &std::process::Output) -> String {
    let mut msg = format!(
        "the {shell} completion script does not parse: `{bin}` exited {}.\n\
         This ships verbatim — `clauth completions install {shell}` writes it and sources it \
         from the user's rc, so a parse error here kills completion for the ENTIRE clauth \
         command, not just the new verb.\n{bin} said:\n{}\n",
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr),
    );
    if shell == "zsh" {
        for (i, line) in script.lines().enumerate() {
            if line.matches('\'').count() % 2 == 1 {
                msg.push_str(&format!(
                    "\nLIKELY CAUSE — odd number of single quotes on script line {}:\n  {line}\n\
                     Every _values/_describe spec in the zsh script is SINGLE-quoted, so an \
                     apostrophe inside a description (a possessive like `profile's`) ends the \
                     string. Reword to drop the apostrophe; do NOT backslash-escape it, since \
                     a backslash is literal inside zsh single quotes.\n",
                    i + 1,
                ));
            }
        }
    }
    msg
}

#[cfg(unix)]
#[test]
fn every_completion_script_parses_in_its_own_shell() {
    // A skip must not be able to masquerade as a pass. nextest (what CI runs)
    // discards a passing test's output, so the eprintln alone would be silent:
    // CI sets CLAUTH_REQUIRE_SHELL_LINT=1 and a missing shell becomes a
    // failure there, while a dev box without fish still gets a useful run.
    let strict = std::env::var("CLAUTH_REQUIRE_SHELL_LINT").as_deref() == Ok("1");
    let mut parsed: Vec<&str> = Vec::new();
    for (shell, bin, args, script, ext) in [
        ("bash", "bash", &["-n"][..], &BASH, ".bash"),
        ("zsh", "zsh", &["-n"][..], &ZSH, ".zsh"),
        ("fish", "fish", &["--no-execute"][..], &FISH, ".fish"),
    ] {
        match parse_check(bin, args, script, ext) {
            None => {
                assert!(
                    !strict,
                    "{bin} is not installed, but CLAUTH_REQUIRE_SHELL_LINT=1 demands it"
                );
                eprintln!("SKIP: {bin} not installed; the {shell} script was NOT parsed");
            }
            Some(out) => {
                parsed.push(shell);
                assert!(
                    out.status.success(),
                    "{}",
                    lint_failure(shell, bin, script, &out)
                );
            }
        }
    }
    // Floor: on a host with no shell at all this test would otherwise pass
    // three times over having parsed nothing.
    assert!(
        parsed.contains(&"bash"),
        "no shell parsed anything — this leg proved nothing"
    );
}

/// Proof the leg can actually fail. Without this, a refactor that swallows the
/// exit status turns it into a permanently green no-op and the class silently
/// reopens — which is exactly how the first one shipped.
#[cfg(unix)]
#[test]
fn the_zsh_leg_would_catch_an_apostrophe_in_a_description() {
    let broken = concat!(
        "_clauth() {\n",
        "    _values 'subcommand' 'feed[feed a profile's session token]'\n",
        "}\n"
    );
    match parse_check("zsh", &["-n"], broken, ".zsh") {
        None => eprintln!("SKIP: zsh not installed; the mutation guard did not run"),
        Some(out) => assert!(
            !out.status.success(),
            "`zsh -n` accepted an apostrophe inside a single-quoted _values spec, so the real \
             leg would not have caught the CLA-ROLL regression either"
        ),
    }
}

/// Portable backstop: runs even on the Windows leg and on a shell-less host.
/// Scoped to ZSH deliberately — the same parity scan over FISH false-positives
/// on `-d "Launch claude with that profile's runtime"`, where the apostrophe is
/// legitimately inside DOUBLE quotes.
///
/// `'"'"'` is the one way to get an apostrophe INTO a single-quoted zsh word
/// and it is balanced by construction — close the string, emit a double-quoted
/// `'`, reopen — so it is elided before counting rather than counted as the
/// three quotes it spells. Without that the scan reds on correct zsh, which is
/// what a merge proved: this guard and the `herdr` rows that use the idiom
/// landed from two branches, each green alone. `zsh -n` above is the leg that
/// judges everything this parity check cannot.
#[test]
fn no_zsh_spec_line_has_an_unbalanced_single_quote() {
    for (i, line) in ZSH.lines().enumerate() {
        assert!(
            zsh_quote_parity_holds(line),
            "odd number of single quotes on ZSH line {}: {line}",
            i + 1
        );
    }
}

/// One spelling, so the negative leg below pins what the scan above runs rather
/// than a copy of it.
fn zsh_quote_parity_holds(line: &str) -> bool {
    line.replace("'\"'\"'", "")
        .matches('\'')
        .count()
        .is_multiple_of(2)
}

/// The elision must not become a hole the original class hides in: an
/// apostrophe pasted straight into a `_values` spec still reds, on a line
/// carrying the idiom as much as on one that does not. Without this leg the
/// scan passes for the two reasons that read alike, and only one of them is
/// the guard working.
#[test]
fn the_parity_scan_still_catches_a_bare_apostrophe_beside_the_escape_idiom() {
    assert!(
        !zsh_quote_parity_holds(
            "    _values 'subcommand' 'install[wire it into herdr'\"'\"'s config]' \
             'feed[feed a profile's session token]'"
        ),
        "the elision swallowed a real unbalanced quote, so the backstop proves nothing"
    );
    assert!(
        zsh_quote_parity_holds(
            "    _values 'subcommand' 'install[wire it into herdr'\"'\"'s config]'"
        ),
        "the idiom alone is balanced zsh and must not red"
    );
}
