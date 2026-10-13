#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{BellLatch, command_argv, message};

#[test]
fn the_message_names_the_account_and_its_trimmed_percent() {
    assert_eq!(message("work", 95.0), "alert: work at 95%");
    assert_eq!(message("a.b@c", 95.5), "alert: a.b@c at 95.5%");
}

#[test]
fn a_fresh_crossing_rings_once_while_it_holds() {
    let mut latch = BellLatch::default();
    assert_eq!(
        latch.observe("alpha", Some(90.0), Some(95.0), true),
        Some("alert: alpha at 95%".to_string())
    );
    assert!(latch.is_ringing("alpha"));
    assert_eq!(latch.observe("alpha", Some(90.0), Some(97.0), true), None);
    assert!(latch.is_ringing("alpha"));
}

#[test]
fn reaching_the_threshold_exactly_rings() {
    let mut latch = BellLatch::default();
    assert_eq!(
        latch.observe("alpha", Some(90.0), Some(90.0), true),
        Some("alert: alpha at 90%".to_string())
    );
}

#[test]
fn falling_below_re_arms_the_next_crossing() {
    let mut latch = BellLatch::default();
    latch.observe("alpha", Some(90.0), Some(95.0), true);
    assert_eq!(latch.observe("alpha", Some(90.0), Some(10.0), true), None);
    assert!(!latch.is_ringing("alpha"));
    assert_eq!(
        latch.observe("alpha", Some(90.0), Some(92.0), true),
        Some("alert: alpha at 92%".to_string())
    );
}

#[test]
fn a_stale_reading_neither_rings_nor_clears() {
    let mut latch = BellLatch::default();
    assert_eq!(latch.observe("alpha", Some(90.0), Some(95.0), false), None);
    assert!(!latch.is_ringing("alpha"));

    latch.observe("alpha", Some(90.0), Some(95.0), true);
    assert_eq!(latch.observe("alpha", Some(90.0), Some(5.0), false), None);
    assert!(
        latch.is_ringing("alpha"),
        "a stale low reading keeps the bell"
    );
    assert_eq!(latch.observe("alpha", Some(90.0), Some(95.0), true), None);
}

#[test]
fn no_threshold_or_no_window_leaves_the_latch_alone() {
    let mut latch = BellLatch::default();
    latch.observe("alpha", Some(90.0), Some(95.0), true);
    assert_eq!(latch.observe("alpha", None, Some(5.0), true), None);
    assert!(latch.is_ringing("alpha"));
    assert_eq!(latch.observe("alpha", Some(90.0), None, true), None);
    assert!(latch.is_ringing("alpha"));
    assert_eq!(latch.observe("beta", None, Some(99.0), true), None);
    assert!(!latch.is_ringing("beta"));
}

#[test]
fn accounts_latch_independently() {
    let mut latch = BellLatch::default();
    latch.observe("alpha", Some(90.0), Some(95.0), true);
    assert_eq!(
        latch.observe("beta", Some(50.0), Some(60.0), true),
        Some("alert: beta at 60%".to_string())
    );
    latch.observe("alpha", Some(90.0), Some(1.0), true);
    assert!(!latch.is_ringing("alpha"));
    assert!(latch.is_ringing("beta"));
}

#[test]
fn the_message_stays_one_argument() {
    assert_eq!(
        command_argv("notify-send %s", "alert: work at 95%").unwrap(),
        ["notify-send", "alert: work at 95%"]
    );
}

#[test]
fn quoted_words_split_by_shell_rules_and_percent_s_substitutes_inside_a_word() {
    assert_eq!(
        command_argv(
            "notify-send 'clauth alert' --body=%s \"two words\"",
            "alert: w at 9%"
        )
        .unwrap(),
        [
            "notify-send",
            "clauth alert",
            "--body=alert: w at 9%",
            "two words"
        ]
    );
}

#[test]
fn an_explicit_shell_gets_the_message_inside_its_script() {
    assert_eq!(
        command_argv("sh -c 'logger %s'", "alert: w at 9%").unwrap(),
        ["sh", "-c", "logger alert: w at 9%"]
    );
}

#[test]
fn quote_and_dollar_characters_in_the_message_reach_the_program_verbatim() {
    assert_eq!(
        command_argv("echo %s", "alert: o'k$(x)\"y at 9%").unwrap(),
        ["echo", "alert: o'k$(x)\"y at 9%"]
    );
}

#[test]
fn a_template_without_percent_s_runs_unchanged() {
    assert_eq!(
        command_argv("beep -f 440", "ignored").unwrap(),
        ["beep", "-f", "440"]
    );
}

#[test]
fn an_unbalanced_quote_is_refused_by_name() {
    let err = command_argv("notify-send 'oops %s", "m").unwrap_err();
    assert_eq!(
        err.to_string(),
        "bell_command has an unbalanced quote or a trailing backslash"
    );
}

#[test]
fn a_blank_template_is_refused() {
    for template in ["", "   "] {
        let err = command_argv(template, "m").unwrap_err();
        assert_eq!(err.to_string(), "bell_command is empty");
    }
}

#[test]
fn a_missing_program_fails_naming_it() {
    let err = super::run_command("clauth-no-such-notifier-7f3a %s", "m").unwrap_err();
    assert_eq!(
        err.to_string(),
        "starting bell_command program \"clauth-no-such-notifier-7f3a\""
    );
}

#[cfg(unix)]
#[test]
fn the_program_runs_with_the_message_as_its_argument() {
    let dir = tempfile::tempdir().unwrap();
    let template = format!(
        "touch {}",
        shlex::try_quote(dir.path().join("%s").to_str().unwrap()).unwrap()
    );
    super::run_command(&template, "alert: alpha at 95%").unwrap();
    let marker = dir.path().join("alert: alpha at 95%");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the bell program never ran"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn a_hung_program_never_blocks_the_caller() {
    let started = std::time::Instant::now();
    super::run_command("sleep 10", "m").unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "run_command waited on its child"
    );
}
