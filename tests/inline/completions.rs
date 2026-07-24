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

/// Drift guard (TECH-14 #23): every user-facing top-level subcommand must appear
/// as a first-token completion in ALL three shells. Add a `clauth <verb>` dispatch
/// arm in main.rs without updating completions and this fails. (Hidden/internal
/// arms — `mcp`, `mcp-await-job`, `__complete` — are intentionally excluded.)
#[test]
fn completions_cover_every_user_facing_subcommand() {
    let subcommands = [
        "start",
        "login",
        "which",
        "status",
        "daemon",
        "doctor",
        "completions",
    ];
    for (shell, script) in [("bash", BASH), ("zsh", ZSH), ("fish", FISH)] {
        for sub in subcommands {
            assert!(
                script.contains(sub),
                "{shell} completions must mention the `{sub}` subcommand",
            );
        }
    }
}

/// The `login` description must reflect reality — browser OAuth, NOT the stale
/// "via claude /login" (clauth runs its own PKCE flow; CC's `/login` on macOS
/// lands only in a per-config-dir Keychain item and leaves the profile empty).
#[test]
fn login_completion_description_is_the_browser_oauth_reality() {
    for (shell, script) in [("bash", BASH), ("zsh", ZSH), ("fish", FISH)] {
        assert!(
            !script.contains("claude /login"),
            "{shell} completions must not describe login as `claude /login`",
        );
    }
    assert!(ZSH.contains("browser OAuth"));
    assert!(FISH.contains("browser OAuth"));
}

/// Every shell's script must offer `--isolated` under the `start` subcommand —
/// it's a documented `clauth start` flag (`main.rs`) and was previously uncovered.
#[test]
fn every_shell_completes_start_isolated_flag() {
    // (script body, the flag token as each shell spells its `start` branch)
    let cases = [
        (
            BASH,
            "\"${COMP_WORDS[1]}\" = \"start\" ] && [ \"${cur:0:2}\" = \"--\"",
        ),
        (
            ZSH,
            "\"${words[2]}\" == start ]] && _values 'flag' '--isolated",
        ),
        (FISH, "__fish_seen_subcommand_from start\" -a --isolated"),
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

/// Every shell must offer `--setup-token` under the `login` subcommand — the
/// long-lived-token capture flow (#53), gated to login like the other login
/// flags. Mirrors the `--isolated` coverage above.
#[test]
fn every_shell_completes_login_setup_token_flag() {
    let cases = [
        (
            BASH,
            "--base-url --api-key --model --new --codex --browser --setup-token",
        ),
        (ZSH, "'--setup-token[capture a claude setup-token"),
        (FISH, "__fish_seen_subcommand_from login\" -a --setup-token"),
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

/// The scripts are hand-written (clap_complete's stable generator can't
/// reproduce the live `clauth __complete` profile-name shellout), so nothing
/// structural keeps them level with the grammar — they had already drifted three
/// subcommands and a root flag behind it. This walks the real clap `Command`
/// tree and fails on the next drift instead of waiting for someone to notice.
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

    let mut missing: Vec<String> = Vec::new();
    for (shell, script) in [("bash", BASH), ("zsh", ZSH), ("fish", FISH)] {
        for (owner, token) in &expected {
            // A bare `contains` would let `--standby` pass on `--no-standby`
            // alone, so match the token with no `-`/alphanumeric neighbour.
            if !offers_token(script, token) {
                missing.push(format!("{shell}: {owner} → {token}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "completion scripts have drifted from the clap grammar:\n  {}",
        missing.join("\n  ")
    );
}

/// Whether `script` offers `token` as a whole word. `--rescue` must not match on
/// `--no-rescue`, nor `start` on `--setup-token`.
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

#[test]
fn offers_token_does_not_match_inside_a_longer_flag() {
    assert!(offers_token("a --rescue b", "--rescue"));
    assert!(!offers_token("a --no-rescue b", "--rescue"));
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
