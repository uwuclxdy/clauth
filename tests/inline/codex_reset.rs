use super::*;

fn credit(
    id: &str,
    reset_type: &str,
    status: &str,
    granted: &str,
    expires: Option<&str>,
) -> ResetCredit {
    ResetCredit {
        id: id.to_string(),
        reset_type: reset_type.to_string(),
        status: status.to_string(),
        granted_at: Some(granted.to_string()),
        expires_at: expires.map(str::to_string),
        title: None,
    }
}

fn list_of(credits: Vec<ResetCredit>, available_count: i64) -> ResetCredits {
    ResetCredits {
        credits,
        available_count,
    }
}

/// The `--list` note's local-stamp fallback, computed in the test so the pin
/// names the instant and not the zone's digits.
fn local_at(sent_at_ms: u64) -> String {
    crate::format::local_stamp((sent_at_ms / 1000) as i64).unwrap_or_else(|| sent_at_ms.to_string())
}

/// The re-send window's end, the same derivation the copy uses.
fn resend_deadline(sent_at_ms: u64) -> String {
    crate::format::local_stamp(((sent_at_ms.saturating_add(600_000)) / 1000) as i64)
        .unwrap_or_else(|| sent_at_ms.to_string())
}

/// The list body codex's own contract test pins, extra fields and all: every
/// field codex reads parses, and the ones it ignores are ignored here too.
#[test]
fn limit_reset_list_parses_codexs_body_and_ignores_the_rest() {
    let body = r#"{
        "credits": [
            {"id": "credit-1", "reset_type": "codex_rate_limits", "status": "available",
             "granted_at": "2026-06-17T00:00:00Z", "expires_at": "2026-07-17T00:00:00Z",
             "redeem_started_at": null, "redeemed_at": null,
             "profile_image_url": "https://example.test/avatar.png", "profile_user_id": "@friend",
             "title": "Full reset (Weekly + 5 hr)", "description": "Ready to redeem"},
            {"id": "credit-2", "reset_type": "codex_rate_limits", "status": "available",
             "granted_at": "2026-06-18T00:00:00Z", "expires_at": null}
        ],
        "available_count": 2,
        "total_earned_count": 4
    }"#;
    let parsed: ResetCredits = serde_json::from_str(body).expect("codex's body parses");
    assert_eq!(parsed.available_count, 2);
    assert_eq!(parsed.credits.len(), 2);
    assert_eq!(
        parsed.credits[0].title.as_deref(),
        Some("Full reset (Weekly + 5 hr)")
    );
    assert_eq!(parsed.credits[1].expires_at, None);

    // A credit the backend describes sparsely still lists, and reads unusable.
    let sparse: ResetCredits =
        serde_json::from_str(r#"{"credits": [{"id": "c"}]}"#).expect("sparse parses");
    assert_eq!(sparse.available_count, 0);
    assert!(
        matches!(
            sparse.try_spend_credit_id("c"),
            Err(CreditRefusal::NotAvailable(_))
        ),
        "no status is not 'available'"
    );
}

/// Any available credit is the one its id names, whatever it expires or is
/// typed; a credit that does not read available refuses as such, and an id
/// the list does not name, or an empty list, refuses as not listed.
#[test]
fn limit_reset_takes_any_available_credit_its_id_names() {
    let credits = list_of(
        vec![
            credit(
                "redeemed",
                "codex_rate_limits",
                "redeemed",
                "2026-01-01T00:00:00Z",
                Some("2026-01-02T00:00:00Z"),
            ),
            credit(
                "redeeming",
                "codex_rate_limits",
                "redeeming",
                "2026-01-01T00:00:00Z",
                Some("2026-01-02T00:00:00Z"),
            ),
            credit(
                "other-type",
                "something_else",
                "available",
                "2026-01-01T00:00:00Z",
                Some("2026-01-03T00:00:00Z"),
            ),
            credit(
                "no-expiry",
                "codex_rate_limits",
                "available",
                "2026-01-01T00:00:00Z",
                None,
            ),
            credit(
                "late",
                "codex_rate_limits",
                "available",
                "2026-01-01T00:00:00Z",
                Some("2026-03-01T00:00:00Z"),
            ),
            credit(
                "soon-granted-later",
                "codex_rate_limits",
                "available",
                "2026-02-01T00:00:00Z",
                Some("2026-02-01T00:00:00+00:00"),
            ),
            credit(
                "soon-granted-first",
                "codex_rate_limits",
                "available",
                "2026-01-15T00:00:00Z",
                Some("2026-02-01T00:00:00Z"),
            ),
        ],
        5,
    );
    for id in [
        "other-type",
        "no-expiry",
        "late",
        "soon-granted-later",
        "soon-granted-first",
    ] {
        assert_eq!(
            credits.try_spend_credit_id(id).map(|c| c.id.as_str()).ok(),
            Some(id),
            "an available credit is spent by its id, another type included"
        );
    }
    for id in ["redeemed", "redeeming"] {
        match credits.try_spend_credit_id(id) {
            Err(CreditRefusal::NotAvailable(c)) => assert_eq!(c.id, id),
            other => panic!("{id}: {other:?}"),
        }
    }
    assert_eq!(credits.available(), 5);
    assert!(matches!(
        credits.try_spend_credit_id("gone"),
        Err(CreditRefusal::NotListed)
    ));
    assert!(matches!(
        list_of(Vec::new(), 0).try_spend_credit_id("gone"),
        Err(CreditRefusal::NotListed)
    ));
}

/// `reset` and `already_redeemed` are both done (codex reads them so);
/// `nothing_to_reset` spent nothing; a code codex does not define is kept
/// verbatim rather than failing to parse after the credit may be gone.
#[test]
fn limit_reset_consume_codes_map_to_their_outcomes() {
    let reply = |body: &str| serde_json::from_str::<ConsumeReply>(body).expect("parses");
    assert_eq!(
        reply(r#"{"code": "reset", "credit": {"id": "ignored"}, "windows_reset": 2}"#).outcome(),
        ConsumeOutcome::Reset { windows_reset: 2 }
    );
    assert_eq!(
        reply(r#"{"code": "already_redeemed"}"#).outcome(),
        ConsumeOutcome::Reset { windows_reset: 0 },
        "windows_reset defaults to 0"
    );
    assert_eq!(
        reply(r#"{"code": "nothing_to_reset"}"#).outcome(),
        ConsumeOutcome::NothingToReset
    );
    assert_eq!(
        reply(r#"{"code": "no_credit"}"#).outcome(),
        ConsumeOutcome::NoCredit
    );
    assert_eq!(
        reply(r#"{"code": "partially_reset", "windows_reset": 1}"#).outcome(),
        ConsumeOutcome::Unknown("partially_reset".to_string())
    );
    assert!(
        serde_json::from_str::<ConsumeReply>(r#"{"windows_reset": 1}"#).is_err(),
        "a reply with no code is not a reply"
    );
}

/// The reason a refusal hands its surface is the code the server answered
/// with, terminal-safe; a reply that landed, or a code clauth does not know,
/// names no refusal.
#[test]
fn limit_reset_a_refused_consume_names_the_reason_the_modal_shows() {
    let reason = |body: &str| {
        serde_json::from_str::<ConsumeReply>(body)
            .expect("parses")
            .refusal_reason()
    };
    assert_eq!(
        reason(r#"{"code": "nothing_to_reset"}"#).as_deref(),
        Some("nothing_to_reset")
    );
    assert_eq!(
        reason(r#"{"code": "no_credit"}"#).as_deref(),
        Some("no_credit")
    );
    for body in [
        r#"{"code": "reset"}"#,
        r#"{"code": "already_redeemed"}"#,
        r#"{"code": "partially_reset"}"#,
        // An unknown code, even one spelled near a refusal, leaves the outcome
        // open: it is never shown as a refusal, so its bytes never reach the
        // modal through this path.
        r#"{"code": "no\u001b[2Jcredit"}"#,
    ] {
        assert_eq!(reason(body), None, "{body}");
    }
}

/// What each outcome prints or fails with. A success is one `clauth: ` line
/// (a caller shows it as it stands); only a success says the reset was
/// used, and every uncertain failure names the re-run that re-sends it.
#[test]
fn limit_reset_outcome_lines_say_what_was_spent() {
    let sent_at_ms = 1_700_000_000_000;
    let credits = list_of(
        (0..3)
            .map(|i| {
                credit(
                    &format!("c-{i}"),
                    "codex_rate_limits",
                    "available",
                    "2026-01-01T00:00:00Z",
                    None,
                )
            })
            .collect(),
        3,
    );
    let reply = |code: &str, windows_reset: i64| ConsumeReply {
        code: code.to_string(),
        windows_reset,
    };

    assert_eq!(
        outcome_line("work", &credits, &reply("reset", 2), sent_at_ms),
        Ok("clauth: used a usage-limit reset on 'work': 2 windows reopened, 2 left.".to_string())
    );
    assert_eq!(
        outcome_line(
            "work",
            &list_of(
                vec![credit(
                    "c",
                    "codex_rate_limits",
                    "available",
                    "2026-01-01T00:00:00Z",
                    None,
                )],
                1,
            ),
            &reply("already_redeemed", 1),
            sent_at_ms,
        ),
        Ok("clauth: used a usage-limit reset on 'work': 1 window reopened, 0 left.".to_string())
    );
    assert_eq!(
        outcome_line("work", &credits, &reply("nothing_to_reset", 0), sent_at_ms),
        Err("there is nothing to reset on 'work' right now, so no reset was used".to_string())
    );
    let no_credit =
        outcome_line("work", &credits, &reply("no_credit", 0), sent_at_ms).expect_err("fails");
    assert!(no_credit.contains("no longer available"), "{no_credit}");
    assert!(
        no_credit.contains("clauth limit-reset work --list"),
        "{no_credit}"
    );
    let unknown =
        outcome_line("work", &credits, &reply("weird\u{1b}[2J", 0), sent_at_ms).expect_err("fails");
    assert_eq!(
        unknown,
        format!(
            "codex answered the reset request for 'work' with an unrecognized code \
             \"weird\\u{{1b}}[2J\", so the outcome is unconfirmed; a re-run before {} re-sends \
             the same request",
            resend_deadline(sent_at_ms)
        )
    );
}

/// The send time every unknown-code case below hands `outcome_line`.
const UNKNOWN_SENT_AT_MS: u64 = 1_700_000_000_000;

/// The message around an unknown code, so each case below pins its own `shown`
/// against everything else the line says.
fn unknown_code_line(shown: &str) -> String {
    format!(
        "codex answered the reset request for 'work' with an unrecognized code {shown}, \
         so the outcome is unconfirmed; a re-run before {} re-sends the same request",
        resend_deadline(UNKNOWN_SENT_AT_MS)
    )
}

/// The unknown-code outcome for `code`, sent at [`UNKNOWN_SENT_AT_MS`].
fn unknown_code_outcome(code: String) -> String {
    let reply = ConsumeReply {
        code,
        windows_reset: 0,
    };
    outcome_line("work", &list_of(Vec::new(), 3), &reply, UNKNOWN_SENT_AT_MS).expect_err("fails")
}

/// An over-cap code keeps its control characters visible as their escapes: each
/// character is escaped on its own before the cut, so the cut can neither drop
/// one (the cell cut's grapheme walk skips control graphemes) nor split one.
/// The `\n` and `\u{1b}` escapes take the kept body to 28 cells, nine y's fill
/// the 37-cell content budget, and the dots sit outside the quoted part.
#[test]
fn limit_reset_unknown_code_keeps_its_control_characters_through_the_cut() {
    let code = format!("{}\n\u{1b}{}", "x".repeat(20), "y".repeat(30));
    let unknown = unknown_code_outcome(code);
    assert_eq!(
        unknown,
        unknown_code_line(&format!(
            "\"{}\\n\\u{{1b}}{}\"...",
            "x".repeat(20),
            "y".repeat(9)
        )),
        "the newline and the escape stay visible through the cut"
    );
}

/// Each character is escaped on its own before the cut, so the cut can neither
/// drop a control character nor split an escape, and the dots sit outside the
/// quoted part. With k x's, the two-cell `\n` escape and twenty y's behind it,
/// the 37-cell content budget keeps 35−k y's; at 36 x's the `\n` piece no
/// longer fits and the cut stops at the last x.
#[test]
fn limit_reset_unknown_code_is_cut_before_it_is_escaped() {
    for (k, ys) in [(30, 5), (31, 4), (32, 3), (33, 2), (34, 1), (35, 0)] {
        let code = format!("{}\n{}", "x".repeat(k), "y".repeat(20));
        let unknown = unknown_code_outcome(code);
        assert_eq!(
            unknown,
            unknown_code_line(&format!("\"{}\\n{}\"...", "x".repeat(k), "y".repeat(ys))),
            "{k} x's before the newline"
        );
    }
    // The `\n` escape needs 2 cells of the 1 the budget has left at 36 x's, so
    // the piece is left out whole and the cut stops at the last x.
    let code = format!("{}\n{}", "x".repeat(36), "y".repeat(20));
    let unknown = unknown_code_outcome(code);
    assert_eq!(
        unknown,
        unknown_code_line(&format!("\"{}\"...", "x".repeat(36))),
        "36 x's leave the newline's escape no room"
    );
}

/// A code far past the cap costs only the cap: the line a 4 MiB code draws is
/// the one its first 64 KiB draws, holding the 37 characters the content budget
/// takes and the dots outside the quotes.
#[test]
fn limit_reset_unknown_code_cuts_a_huge_code_like_its_prefix() {
    let huge = "x".repeat(4 * 1024 * 1024);
    let line = unknown_code_outcome;
    let expected = unknown_code_line(&format!("\"{}\"...", "x".repeat(37)));
    assert_eq!(line(huge.clone()), expected);
    assert_eq!(line(huge[..64 * 1024].to_string()), expected);
}

/// The escape is `{:?}`: a literal backslash in the code comes back doubled, so
/// a code holding `\u{000a}` cannot read as the newline's own escape, which is
/// the single-backslash `\n` pinned beside it.
#[test]
fn limit_reset_unknown_code_keeps_a_literal_backslash_apart_from_an_escape() {
    let line = unknown_code_outcome;

    assert_eq!(
        line(format!("{}\\u{{000a}}", "x".repeat(30))),
        unknown_code_line(&format!("\"{}\\\\u{{000a}}\"", "x".repeat(30))),
        "a literal backslash-u sequence stays doubled"
    );
    assert_eq!(
        line("a\nb".to_string()),
        unknown_code_line(r#""a\nb""#),
        "a real newline is the one-backslash short escape"
    );
    assert_eq!(
        line("it's".to_string()),
        unknown_code_line(r#""it's""#),
        "inside double quotes an apostrophe prints bare, as a string's Debug form prints it"
    );
}

/// A failed list spent nothing and says so, whatever the failure; a failed
/// consume says so only on a 401 — every other failure leaves the outcome open.
#[test]
fn limit_reset_failure_lines_separate_nothing_spent_from_unconfirmed() {
    let rejected_line = "codex rejected the stored access token for 'work'. The daemon \
                         refreshes it for a parked account and codex does for the one in use. \
                         Try again after that. No reset was used";
    for (err, want) in [
        (ResetCallError::Unauthorized, rejected_line),
        (
            ResetCallError::Status(503),
            "codex answered the reset list for 'work' with HTTP 503; no reset was used",
        ),
        (
            ResetCallError::Transport,
            "could not reach codex to list the resets on 'work'; no reset was used",
        ),
        (
            ResetCallError::Parse,
            "codex answered the reset list for 'work' in a shape clauth does not read; \
             no reset was used",
        ),
    ] {
        assert_eq!(list_failure("work", &err), want, "{err:?}");
    }

    let sent_at_ms = 1_780_741_000_000u64;
    let deadline = resend_deadline(sent_at_ms);
    assert_eq!(
        consume_failure("work", &ResetCallError::Unauthorized, sent_at_ms, false),
        rejected_line
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Transport, sent_at_ms, false),
        format!(
            "the reset request for 'work' got no answer, so the reset may or may not have gone \
             through; a re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Status(500), sent_at_ms, false),
        format!(
            "codex answered the reset request for 'work' with HTTP 500; a re-run before \
             {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Parse, sent_at_ms, false),
        format!(
            "codex answered the reset request for 'work' in a shape clauth does not read, so the \
             reset may or may not have gone through; a re-run before {deadline} re-sends the same \
             request"
        )
    );
}

/// A codex re-send's failed consume says the earlier request's outcome is still
/// unknown, whatever refused this one, and names the window's end.
#[test]
fn a_codex_resend_failure_keeps_the_earlier_outcome_open() {
    let sent_at_ms = 1_780_741_000_000u64;
    let deadline = resend_deadline(sent_at_ms);
    let open = "the earlier request's outcome is still unknown";
    assert_eq!(
        consume_failure("work", &ResetCallError::Unauthorized, sent_at_ms, true),
        format!(
            "codex rejected the stored access token for 'work'. The daemon refreshes it for a \
             parked account and codex does for the one in use. Try again after that, and {open}; \
             a re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Status(409), sent_at_ms, true),
        format!(
            "codex answered the re-sent reset request for 'work' with HTTP 409; {open}; a \
             re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Status(503), sent_at_ms, true),
        format!(
            "codex answered the re-sent reset request for 'work' with HTTP 503; {open}; a \
             re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Transport, sent_at_ms, true),
        format!(
            "the re-sent reset request for 'work' got no answer; {open}; a re-run before \
             {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        consume_failure("work", &ResetCallError::Parse, sent_at_ms, true),
        format!(
            "codex answered the re-sent reset request for 'work' in a shape clauth does not \
             read; {open}; a re-run before {deadline} re-sends the same request"
        )
    );
}

/// An untitled credit's prompt names no credit: the question already says a
/// usage-limit reset is spent, so the expiry clause opens the next sentence.
/// A titled credit keeps its title in front.
#[test]
fn limit_reset_prompt_opens_on_the_expiry_when_the_credit_has_no_title() {
    let untitled = credit(
        "u",
        "codex_rate_limits",
        "available",
        "2026-01-01T00:00:00Z",
        None,
    );
    let mut titled = untitled.clone();
    titled.id = "t".to_string();
    titled.title = Some("Full reset".to_string());
    let credits = list_of(vec![untitled.clone(), titled.clone()], 2);

    assert_eq!(
        limit_reset_prompt("work", &credits, &untitled),
        "clauth: use a usage-limit reset on 'work'? No expiry, 1 of 2 available. \
         It reopens the account's usage windows now and cannot be undone."
    );
    assert_eq!(
        limit_reset_prompt("work", &credits, &titled),
        "clauth: use a usage-limit reset on 'work'? Full reset, no expiry, 1 of 2 available. \
         It reopens the account's usage windows now and cannot be undone."
    );
}

/// The prompt for one credit, the only one in its list, under a server count
/// (3) that differs from the list's own, so the "1 of N" figure is pinned to
/// the list's own available credits and never to the server's count.
fn sole_prompt(credit: &ResetCredit) -> String {
    limit_reset_prompt("work", &list_of(vec![credit.clone()], 3), credit)
}

/// The listing's table row for one credit, the only one in its list.
fn sole_list_row(credit: &ResetCredit) -> String {
    describe_reset_credits("work", &list_of(vec![credit.clone()], 3))[3].clone()
}

/// The table row of a lone credit with a one-letter id, no expiry and the
/// `codex_rate_limits` type, padded by hand: every column at its header's
/// width but RESETS and EXPIRES, which their cells widen, and STATE, which a
/// cell past five cells widens.
fn lone_row(id: &str, left: &str, state: &str, label: &str) -> String {
    format!("  {id}   {left}   {state:<5}  usage windows  no expiry  {label}")
}

/// A credit whose grant stamp does not parse, so its `--list` row is shown
/// verbatim and every byte of it can be pinned.
fn fixed_credit(expires_at: Option<&str>) -> ResetCredit {
    credit(
        "a",
        "codex_rate_limits",
        "available",
        "not-a-stamp",
        expires_at,
    )
}

const PROMPT_TAIL: &str =
    ", 1 of 1 available. It reopens the account's usage windows now and cannot be undone.";

fn titled_credit(title: &str) -> ResetCredit {
    ResetCredit {
        title: Some(title.to_string()),
        ..fixed_credit(None)
    }
}

/// What the server sends is shown as sent, minus what would hide or reorder
/// the confirm line: a lowercase title stays lowercase, a title that is only
/// blanks and control characters is no title, a long one is capped, and the
/// bidi controls that reorder a terminal line are dropped.
#[test]
fn limit_reset_prompt_shows_the_title_as_sent_minus_what_hides_the_line() {
    let titled = titled_credit;
    let tail = PROMPT_TAIL;

    assert_eq!(
        sole_prompt(&titled("weekly reset")),
        format!("clauth: use a usage-limit reset on 'work'? weekly reset, no expiry{tail}")
    );

    let blank = titled(" \u{7} ");
    assert_eq!(
        sole_prompt(&blank),
        format!("clauth: use a usage-limit reset on 'work'? No expiry{tail}")
    );
    assert_eq!(
        sole_list_row(&blank),
        lone_row("a", "1/1", "ready", "usage-limit reset")
    );

    let long = titled(&"x".repeat(70));
    assert_eq!(
        sole_prompt(&long),
        format!(
            "clauth: use a usage-limit reset on 'work'? {}..., no expiry{tail}",
            "x".repeat(57)
        )
    );

    let bidi = titled("Full\u{202e} reset\u{2066}");
    assert_eq!(
        sole_prompt(&bidi),
        format!("clauth: use a usage-limit reset on 'work'? Full reset, no expiry{tail}")
    );
    assert_eq!(
        sole_list_row(&bidi),
        lone_row("a", "1/1", "ready", "Full reset")
    );
}

/// A title a terminal draws as nothing is no title, whichever invisible
/// characters it is made of; one visible character keeps it a title, shown
/// as sent.
#[test]
fn limit_reset_invisible_title_reads_as_untitled() {
    let titled = titled_credit;
    for invisible in [
        "\u{200b}",
        "\u{3164}",
        "\u{feff}\u{2060}",
        " \u{e0041} \u{ad}",
    ] {
        let c = titled(invisible);
        assert_eq!(
            sole_prompt(&c),
            "clauth: use a usage-limit reset on 'work'? No expiry, 1 of 1 available. \
             It reopens the account's usage windows now and cannot be undone.",
            "title {invisible:?}"
        );
        assert_eq!(
            sole_list_row(&c),
            lone_row("a", "1/1", "ready", "usage-limit reset"),
            "title {invisible:?}"
        );
    }
    assert_eq!(
        sole_prompt(&titled("a\u{200b}")),
        "clauth: use a usage-limit reset on 'work'? a\u{200b}, no expiry, 1 of 1 available. \
         It reopens the account's usage windows now and cannot be undone."
    );
}

/// Both ends of every `Default_Ignorable_Code_Point` range, typed from Unicode
/// 18.0.0 `DerivedCoreProperties.txt` rather than from the code: each one
/// alone, as a title, reads untitled.
#[test]
fn limit_reset_every_ignorable_range_end_reads_as_untitled() {
    let ranges: [(u32, u32); 17] = [
        (0x00AD, 0x00AD),
        (0x034F, 0x034F),
        (0x061C, 0x061C),
        (0x115F, 0x1160),
        (0x17B4, 0x17B5),
        (0x180B, 0x180F),
        (0x200B, 0x200F),
        (0x202A, 0x202E),
        (0x2060, 0x206F),
        (0x3164, 0x3164),
        (0xFE00, 0xFE0F),
        (0xFEFF, 0xFEFF),
        (0xFFA0, 0xFFA0),
        (0xFFF0, 0xFFF8),
        (0x1BCA0, 0x1BCA3),
        (0x1D173, 0x1D17A),
        (0xE0000, 0xE0FFF),
    ];
    for (lo, hi) in ranges {
        for cp in [lo, hi] {
            let ch = char::from_u32(cp).expect("a scalar value");
            let c = ResetCredit {
                title: Some(ch.to_string()),
                ..fixed_credit(None)
            };
            assert_eq!(
                sole_prompt(&c),
                "clauth: use a usage-limit reset on 'work'? No expiry, 1 of 1 available. \
                 It reopens the account's usage windows now and cannot be undone.",
                "U+{cp:04X}"
            );
        }
    }
}

/// A listing row reads a status that shows nothing as `-` instead of an empty
/// cell, and a grant stamp never reaches the table; the codex body that sends
/// neither a status nor a type (a credit with only an id) still lists.
#[test]
fn limit_reset_list_row_reads_a_status_that_shows_nothing_as_a_dash() {
    for blank in ["", "   ", "\u{7}", "\u{200b}"] {
        let no_status = credit("c", "codex_rate_limits", blank, "not-a-stamp", None);
        assert_eq!(
            describe_reset_credits("work", &list_of(vec![no_status], 0))[3],
            lone_row("c", "0/1", "-", "usage-limit reset"),
            "status {blank:?}"
        );
        let no_grant = credit("c", "codex_rate_limits", "available", blank, None);
        assert_eq!(
            describe_reset_credits("work", &list_of(vec![no_grant], 1))[3],
            lone_row("c", "1/1", "ready", "usage-limit reset"),
            "granted_at {blank:?}"
        );
    }
    let sparse: ResetCredits =
        serde_json::from_str(r#"{"credits": [{"id": "c"}]}"#).expect("a sparse credit parses");
    assert_eq!(
        describe_reset_credits("work", &sparse)[3],
        "  c   0/1   -      -       no expiry  usage-limit reset"
    );
}

/// An expiry stamp with nothing left to show is no stamp: blank, whitespace,
/// only characters the terminal filter drops, or only characters a terminal
/// draws as nothing. Both the confirm and `--list` say there is no expiry
/// instead of printing an empty clause.
#[test]
fn limit_reset_blank_expiry_reads_as_no_expiry() {
    for blank in [
        "",
        "   ",
        "\u{7}",
        "\u{202e}",
        " \u{1b} ",
        "\u{200b}",
        "\u{2060} \u{fe0f}",
        "\u{e0041}",
    ] {
        let c = fixed_credit(Some(blank));
        assert_eq!(
            sole_prompt(&c),
            "clauth: use a usage-limit reset on 'work'? No expiry, 1 of 1 available. \
             It reopens the account's usage windows now and cannot be undone.",
            "expires_at {blank:?}"
        );
        assert_eq!(
            sole_list_row(&c),
            lone_row("a", "1/1", "ready", "usage-limit reset"),
            "expires_at {blank:?}"
        );
    }
}

/// The prompt names the credit, its expiry, and "1 of N"; the listing names
/// every credit and marks none. A missing title falls back to the generic
/// name in the listing, and the count never reads "1 of 0".
#[test]
fn limit_reset_prompt_names_the_credit_and_the_listing_marks_none() {
    let mut titled = credit(
        "a",
        "codex_rate_limits",
        "available",
        "2026-01-01T00:00:00Z",
        None,
    );
    titled.title = Some("Full reset (Weekly + 5 hr)\u{7}".to_string());
    let dated = credit(
        "b",
        "codex_rate_limits",
        "available",
        "2026-01-01T00:00:00Z",
        Some("2026-02-01T00:00:00Z"),
    );
    let credits = list_of(vec![titled.clone(), dated.clone()], 2);

    let prompt = limit_reset_prompt("work", &credits, &dated);
    assert!(
        prompt.starts_with("clauth: use a usage-limit reset on 'work'? Expires "),
        "{prompt}"
    );
    assert!(prompt.contains(", 1 of 2 available."), "{prompt}");
    assert!(prompt.contains("cannot be undone"), "{prompt}");
    let titled_prompt = limit_reset_prompt("work", &list_of(vec![titled.clone()], 0), &titled);
    assert!(
        titled_prompt.contains("Full reset (Weekly + 5 hr), no expiry, 1 of 1 available."),
        "control characters dropped, the count is the list's own single credit: \
         {titled_prompt}"
    );

    let lines = describe_reset_credits("work", &credits);
    let ends = crate::format::local_minute_stamp(1_769_904_000).expect("stamp");
    let w = ends.chars().count();
    assert_eq!(
        lines,
        [
            "clauth: 'work' has 2 usage-limit resets available.".to_string(),
            String::new(),
            format!("  ID  LEFT  STATE  RESETS         {:<w$}  LABEL", "EXPIRES"),
            format!(
                "  a   1/1   ready  usage windows  {:<w$}  Full reset (Weekly + 5 hr)",
                "no expiry"
            ),
            format!("  b   1/1   ready  usage windows  {ends}  usage-limit reset"),
        ]
    );

    let spent = list_of(
        vec![credit(
            "r",
            "codex_rate_limits",
            "redeemed",
            "2026-01-01T00:00:00Z",
            None,
        )],
        0,
    );
    let lines = describe_reset_credits("work", &spent);
    assert_eq!(
        lines,
        [
            "clauth: 'work' has 0 usage-limit resets available.",
            "",
            "  ID  LEFT  STATE     RESETS         EXPIRES    LABEL",
            "  r   0/1   redeemed  usage windows  no expiry  usage-limit reset",
        ]
    );
    assert_eq!(
        describe_reset_credits("work", &list_of(Vec::new(), 0)),
        ["clauth: no usage-limit resets available on 'work'."]
    );
}

/// The prompt's "1 of N" and the listing's count state the list's own
/// available credits, never the server's count: a list whose server count
/// (2) differs from its one available credit prompts "1 of 1", and the
/// listing's count line agrees.
#[test]
fn limit_reset_prompt_and_listing_agree_on_the_lists_own_count() {
    let c = credit(
        "c",
        "codex_rate_limits",
        "available",
        "2026-01-01T00:00:00Z",
        None,
    );
    let credits = list_of(vec![c.clone()], 2);
    assert_eq!(
        limit_reset_prompt("work", &credits, &c),
        "clauth: use a usage-limit reset on 'work'? No expiry, 1 of 1 available. \
         It reopens the account's usage windows now and cannot be undone."
    );
    assert_eq!(
        describe_reset_credits("work", &credits)[0],
        "clauth: 'work' has 1 usage-limit reset available."
    );
}

/// The codex failed-re-read line names the read's failure and only what the
/// refresh did with the stored list under the gate: dropped, left alone by
/// the account gate or a state flock, or (with the drop-failure line after
/// it) not removed.
#[test]
fn the_codex_refresh_read_failure_line_says_what_became_of_the_stored_list() {
    let err = ResetCallError::Status(503);
    assert_eq!(
        refresh_read_failure("cx", &err, crate::usage::claude_reset::RefreshDrop::Dropped),
        "clauth: 'cx': the reset list read after the spend failed (codex answered with HTTP \
         503); the stored list was dropped"
    );
    assert_eq!(
        refresh_read_failure(
            "cx",
            &err,
            crate::usage::claude_reset::RefreshDrop::LoginChanged
        ),
        "clauth: 'cx': the reset list read after the spend failed (codex answered with HTTP \
         503); the stored login changed or could not be read, so nothing was touched"
    );
    assert_eq!(
        refresh_read_failure(
            "cx",
            &err,
            crate::usage::claude_reset::RefreshDrop::LockTimedOut
        ),
        "clauth: 'cx': the reset list read after the spend failed (codex answered with HTTP \
         503); the state lock was unavailable, so nothing was touched"
    );
    assert_eq!(
        refresh_read_failure("cx", &err, crate::usage::claude_reset::RefreshDrop::Failed),
        "clauth: 'cx': the reset list read after the spend failed (codex answered with HTTP 503)"
    );
    assert_eq!(
        refresh_drop_failure("cx"),
        "clauth: 'cx': could not remove the stored reset list"
    );
}

/// An expiry or grant stamp that does not parse is shown as sent, but with its
/// control characters dropped: an escape sequence in it would otherwise reach
/// the terminal through both the `[y/N]` prompt and `--list`.
#[test]
fn limit_reset_unparseable_stamps_reach_the_terminal_without_control_characters() {
    let hostile = "2026-01-01T00:00:00\u{1b}]52;c;AAAA\u{7}";
    let c = credit(
        "h",
        "codex_rate_limits",
        "available",
        hostile,
        Some(hostile),
    );
    let credits = list_of(vec![c.clone()], 1);

    let prompt = limit_reset_prompt("work", &credits, &c);
    let lines = describe_reset_credits("work", &credits);
    for text in std::iter::once(&prompt).chain(lines.iter()) {
        assert!(!text.chars().any(char::is_control), "{text:?}");
    }
    assert_eq!(
        prompt,
        "clauth: use a usage-limit reset on 'work'? Expires 2026-01-01T00:00:00]52;c;AAAA, \
         1 of 1 available. It reopens the account's usage windows now and cannot be undone."
    );
    assert_eq!(
        lines[3],
        "  h   1/1   ready  usage windows  2026-01-01T00:00:00]52;c;AAAA  usage-limit reset"
    );
}

/// The title's 60 cap counts only the characters that take a cell: a run of
/// zero-width characters inside a title never pushes it past the cap, and none
/// of them is dropped. A cut keeps the invisible characters right after the last drawn one,
/// so an emoji keeps its variation selector. Blanks count, as spaces do; a
/// title exactly at the cap is shown whole.
#[test]
fn limit_reset_title_cap_counts_only_drawn_characters() {
    let padded = format!("x{}y", "\u{200b}".repeat(70));
    assert_eq!(
        sole_prompt(&titled_credit(&padded)),
        format!("clauth: use a usage-limit reset on 'work'? {padded}, no expiry{PROMPT_TAIL}")
    );
    assert_eq!(
        sole_list_row(&titled_credit(&padded)),
        lone_row("a", "1/1", "ready", &padded)
    );

    let heart = "\u{2764}\u{fe0f}";
    // The presentation sequence draws its two cells as one grapheme: 70 of
    // them are 140 cells, and the 57-cell content budget keeps 28 whole pairs.
    assert_eq!(
        sole_prompt(&titled_credit(&heart.repeat(70))),
        format!(
            "clauth: use a usage-limit reset on 'work'? {}..., no expiry{PROMPT_TAIL}",
            heart.repeat(28)
        )
    );

    assert_eq!(
        sole_prompt(&titled_credit(&"x ".repeat(35))),
        format!(
            "clauth: use a usage-limit reset on 'work'? {}x..., no expiry{PROMPT_TAIL}",
            "x ".repeat(28)
        )
    );

    let at_cap = format!(
        "{}{}{}",
        "x".repeat(30),
        "\u{200b}".repeat(5),
        "x".repeat(30)
    );
    assert_eq!(
        sole_prompt(&titled_credit(&at_cap)),
        format!("clauth: use a usage-limit reset on 'work'? {at_cap}, no expiry{PROMPT_TAIL}")
    );
}

/// A wide CJK glyph draws two terminal cells, so the title's 60 cap keeps 28
/// of them (56 cells) where a one-cell count would keep 57.
#[test]
fn limit_reset_title_cap_counts_wide_glyphs_by_cells() {
    assert_eq!(
        sole_prompt(&titled_credit(&"漢".repeat(70))),
        format!(
            "clauth: use a usage-limit reset on 'work'? {}..., no expiry{PROMPT_TAIL}",
            "漢".repeat(28)
        )
    );
}

/// The table's LABEL column caps at 44 drawn cells: a wide title keeps whole
/// glyphs inside that width, a 44-cell one prints whole, and a 45-cell one
/// cuts to 41 and `...`.
#[test]
fn the_listing_cuts_a_label_at_forty_four_drawn_cells() {
    let wide = titled_credit(&"漢".repeat(30));
    assert_eq!(
        sole_list_row(&wide),
        lone_row("a", "1/1", "ready", &format!("{}...", "漢".repeat(20)))
    );
    assert_eq!(
        sole_list_row(&titled_credit(&"x".repeat(44))),
        lone_row("a", "1/1", "ready", &"x".repeat(44))
    );
    assert_eq!(
        sole_list_row(&titled_credit(&"x".repeat(45))),
        lone_row("a", "1/1", "ready", &format!("{}...", "x".repeat(41)))
    );
}

/// The ID cell is the text a spend takes back, so it prints whole at a real
/// codex credit id's length, past any cap the other cells keep: the id read
/// back off the row's first column is the one the spend's lookup finds, and
/// the columns after it still line up under a shorter id's row.
#[test]
fn the_listing_prints_a_codex_credit_id_whole() {
    let long = "RateLimitResetCredit_3f9a1c7e5b2d4f6a8c0e1b3d5f7a9c2e";
    let credits = list_of(
        vec![
            credit(long, "codex_rate_limits", "available", "not-a-stamp", None),
            credit("a", "codex_rate_limits", "available", "not-a-stamp", None),
        ],
        2,
    );
    let lines = describe_reset_credits("work", &credits);
    assert!(lines[3].contains(long), "row {:?} names {long}", lines[3]);
    let read_back = lines[3].split_whitespace().next().expect("an ID cell");
    assert_eq!(
        credits
            .try_spend_credit_id(read_back)
            .map(|c| c.id.as_str())
            .ok(),
        Some(long),
        "the listed id is the one a spend of it finds"
    );
    let left_at = |line: &str| line.find("1/1").expect("a LEFT cell");
    assert_eq!(left_at(&lines[3]), left_at(&lines[4]), "{lines:#?}");
    assert_eq!(left_at(&lines[3]), lines[2].find("LEFT").expect("LEFT"));
}

/// The STATE cell's status cap (20) counts only drawn characters too, and the
/// column pads by drawn cells, so the zero-width characters widen nothing.
#[test]
fn limit_reset_status_cap_counts_only_drawn_characters() {
    let status = format!("re{}deemed", "\u{200b}".repeat(25));
    let c = credit("c", "codex_rate_limits", &status, "not-a-stamp", None);
    assert_eq!(
        describe_reset_credits("work", &list_of(vec![c], 0))[3],
        format!("  c   0/1   {status}  usage windows  no expiry  usage-limit reset")
    );
}

/// An unparseable stamp's cap (40) counts only drawn characters too, on the
/// prompt's expiry and the table's alike.
#[test]
fn limit_reset_raw_stamp_cap_counts_only_drawn_characters() {
    let raw = format!("{}soon", "\u{200b}".repeat(45));
    assert_eq!(
        sole_prompt(&fixed_credit(Some(&raw))),
        format!("clauth: use a usage-limit reset on 'work'? Expires {raw}{PROMPT_TAIL}")
    );
    assert_eq!(
        sole_list_row(&fixed_credit(Some(&raw))),
        format!("  a   1/1   ready  usage windows  {raw}     usage-limit reset")
    );
}

/// U+2800 BRAILLE PATTERN BLANK draws an empty cell, so it is blank like a
/// space: a title, expiry, status or grant made of it shows nothing.
#[test]
fn limit_reset_braille_blank_alone_shows_nothing() {
    let blank = "\u{2800}";
    for text in [blank.to_string(), format!(" {blank} {blank} ")] {
        assert_eq!(
            sole_prompt(&titled_credit(&text)),
            format!("clauth: use a usage-limit reset on 'work'? No expiry{PROMPT_TAIL}"),
            "title {text:?}"
        );
        assert_eq!(
            sole_list_row(&titled_credit(&text)),
            lone_row("a", "1/1", "ready", "usage-limit reset"),
            "title {text:?}"
        );
        assert_eq!(
            sole_prompt(&fixed_credit(Some(&text))),
            format!("clauth: use a usage-limit reset on 'work'? No expiry{PROMPT_TAIL}"),
            "expires_at {text:?}"
        );
        let no_status = credit("c", "codex_rate_limits", &text, "not-a-stamp", None);
        assert_eq!(
            describe_reset_credits("work", &list_of(vec![no_status], 0))[3],
            lone_row("c", "0/1", "-", "usage-limit reset"),
            "status {text:?}"
        );
        let no_grant = credit("c", "codex_rate_limits", "available", &text, None);
        assert_eq!(
            sole_list_row(&no_grant),
            lone_row("c", "1/1", "ready", "usage-limit reset"),
            "granted_at {text:?}"
        );
    }
}

/// A title trims U+2800 off both ends, as it trims spaces.
#[test]
fn limit_reset_title_trims_braille_blanks_off_its_ends() {
    let c = titled_credit("\u{2800} Full reset \u{2800}\u{2800}");
    assert_eq!(
        sole_prompt(&c),
        format!("clauth: use a usage-limit reset on 'work'? Full reset, no expiry{PROMPT_TAIL}")
    );
    assert_eq!(
        sole_list_row(&c),
        lone_row("a", "1/1", "ready", "Full reset")
    );
}

/// The listing's header and the table above a lone credit's row.
fn lone_table(count: &str, row: String) -> Vec<String> {
    vec![
        format!("clauth: 'work' has {count} available."),
        String::new(),
        "  ID  LEFT  STATE  RESETS         EXPIRES    LABEL".to_string(),
        row,
    ]
}

/// A status sent with blanks around `available` is available: it is one a
/// spend of its id consumes, and the listing reads it `ready`.
#[test]
fn limit_reset_spends_an_available_status_sent_with_blanks() {
    for status in [" available ", "\u{2800}available\t"] {
        let c = credit("a", "codex_rate_limits", status, "not-a-stamp", None);
        let credits = list_of(vec![c.clone()], 3);
        assert_eq!(
            credits.try_spend_credit_id("a").ok(),
            Some(&c),
            "status {status:?}"
        );
        assert_eq!(
            describe_reset_credits("work", &credits),
            lone_table(
                "1 usage-limit reset",
                lone_row("a", "1/1", "ready", "usage-limit reset")
            ),
            "status {status:?}"
        );
    }
}

/// The Hangul fillers draw an empty cell, so they are blank like a space: they
/// count toward a cap and trim off the ends.
#[test]
fn limit_reset_hangul_fillers_count_and_trim_as_blank() {
    let filled = format!("a{}b", "\u{3164}".repeat(100));
    assert_eq!(
        sole_prompt(&titled_credit(&filled)),
        format!(
            "clauth: use a usage-limit reset on 'work'? a{}..., no expiry{PROMPT_TAIL}",
            "\u{3164}".repeat(56)
        )
    );
    assert_eq!(
        sole_prompt(&titled_credit("\u{3164}Full reset\u{ffa0}")),
        format!("clauth: use a usage-limit reset on 'work'? Full reset, no expiry{PROMPT_TAIL}")
    );
    assert_eq!(
        sole_prompt(&titled_credit("Full reset\u{115f}")),
        format!("clauth: use a usage-limit reset on 'work'? Full reset, no expiry{PROMPT_TAIL}")
    );
    let c = credit(
        "a",
        "codex_rate_limits",
        "\u{115f}available",
        "not-a-stamp",
        None,
    );
    let credits = list_of(vec![c.clone()], 3);
    assert_eq!(credits.try_spend_credit_id("a").ok(), Some(&c));
    assert_eq!(
        describe_reset_credits("work", &credits)[3],
        lone_row("a", "1/1", "ready", "usage-limit reset")
    );
}

/// A credit is one a spend consumes when the status the listing reads for it
/// is `available`: control, bidi and invisible characters never keep a row
/// that reads `ready` from being spent by its id.
#[test]
fn limit_reset_spends_the_status_list_shows_as_available() {
    for status in ["available\u{7}", "available\u{202e}", "\u{200b}available"] {
        let c = credit("a", "codex_rate_limits", status, "not-a-stamp", None);
        let credits = list_of(vec![c.clone()], 3);
        assert_eq!(
            credits.try_spend_credit_id("a").ok(),
            Some(&c),
            "status {status:?}"
        );
        assert_eq!(
            describe_reset_credits("work", &credits),
            lone_table(
                "1 usage-limit reset",
                lone_row("a", "1/1", "ready", "usage-limit reset")
            ),
            "status {status:?}"
        );
    }
}

/// A title trims invisible padding off its ends along with blanks, and keeps
/// the invisible characters right after its last drawn one.
#[test]
fn limit_reset_title_trims_invisible_padding_off_its_ends() {
    for (title, shown) in [
        (" \u{200b} Full reset \u{200b} ", "Full reset"),
        ("\u{2060}\u{feff}Full reset\u{2060}", "Full reset\u{2060}"),
        ("\u{2764}\u{fe0f} ", "\u{2764}\u{fe0f}"),
    ] {
        assert_eq!(
            sole_prompt(&titled_credit(title)),
            format!("clauth: use a usage-limit reset on 'work'? {shown}, no expiry{PROMPT_TAIL}"),
            "title {title:?}"
        );
    }
}

/// A cap below the ellipsis' own width is dots alone, as `format::truncate`
/// gives, never a panic.
#[test]
fn limit_reset_cap_below_the_ellipsis_is_dots_alone() {
    assert_eq!(cap_drawn("abcd", 2), "..");
    assert_eq!(cap_drawn("abcd", 0), "");
    assert_eq!(cap_drawn("abcd", 3), "...");
    assert_eq!(cap_drawn("a\u{200b}\u{200b}", 2), "a\u{200b}\u{200b}");
}

/// A soft hyphen takes a cell on most terminals (drawn as a hyphen), so it
/// counts toward a cap and a status holding one inside is not `available`;
/// alone or at the ends it is padding, like an invisible character.
#[test]
fn limit_reset_soft_hyphen_takes_a_cell() {
    let c = credit(
        "a",
        "codex_rate_limits",
        "avail\u{ad}able",
        "not-a-stamp",
        None,
    );
    let credits = list_of(vec![c], 0);
    assert!(matches!(
        credits.try_spend_credit_id("a"),
        Err(CreditRefusal::NotAvailable(_))
    ));
    assert_eq!(
        describe_reset_credits("work", &credits),
        vec![
            "clauth: 'work' has 0 usage-limit resets available.".to_string(),
            String::new(),
            "  ID  LEFT  STATE       RESETS         EXPIRES    LABEL".to_string(),
            "  a   0/1   avail\u{ad}able  usage windows  no expiry  usage-limit reset".to_string(),
        ]
    );

    let hyphens = format!("x{}y", "\u{ad}".repeat(70));
    assert_eq!(
        sole_prompt(&titled_credit(&hyphens)),
        format!(
            "clauth: use a usage-limit reset on 'work'? x{}..., no expiry{PROMPT_TAIL}",
            "\u{ad}".repeat(56)
        )
    );

    assert_eq!(
        sole_prompt(&titled_credit("\u{ad}Full reset\u{ad}")),
        format!("clauth: use a usage-limit reset on 'work'? Full reset, no expiry{PROMPT_TAIL}")
    );
}

/// The idempotency key is a v4 UUID: 8-4-4-4-12 lowercase hex, version nibble
/// 4, variant bits 10. Fresh per call.
#[test]
fn limit_reset_redeem_request_id_is_a_fresh_v4_uuid() {
    assert_eq!(
        uuid_v4_from([0xff; 16]),
        "ffffffff-ffff-4fff-bfff-ffffffffffff"
    );
    assert_eq!(
        uuid_v4_from([0; 16]),
        "00000000-0000-4000-8000-000000000000"
    );
    let a = new_redeem_request_id().expect("id");
    let b = new_redeem_request_id().expect("id");
    assert_ne!(a, b);
    assert!(is_v4_uuid(&a), "{a}");
}

fn is_v4_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
        && s.chars()
            .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
        && parts[2].starts_with('4')
        && parts[3].starts_with(['8', '9', 'a', 'b'])
}

/// The wire against a loopback stub: the GET lists, the POST consumes the
/// named credit under a v4 key, and both carry codex's headers — bearer,
/// account id (only when there is one), the parity UA, the fedramp flag only
/// when the account is one, and no `Accept` (codex's backend client sends
/// none).
#[test]
fn limit_reset_list_and_consume_send_codexs_request() {
    let list_body = r#"{"credits": [{"id": "c-1", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-01T00:00:00Z"}], "available_count": 1}"#;
    let (addr, handle) = crate::testutil::serve_endpoints_raw(3, move |path, _i| {
        if path.ends_with("/consume") {
            (200, r#"{"code": "reset", "windows_reset": 2}"#.to_string())
        } else {
            (200, list_body.to_string())
        }
    });
    let urls = ResetUrls::under(&addr);

    let listed =
        list_reset_credits_at(&urls.list, "at.secret", Some("acc-1"), true).expect("200 lists");
    assert_eq!(
        listed
            .try_spend_credit_id("c-1")
            .map(|c| c.id.as_str())
            .ok(),
        Some("c-1")
    );
    let reply = consume_reset_credit_at(
        &urls.consume,
        "at.secret",
        Some("acc-1"),
        false,
        "11111111-2222-4333-8444-555555555555",
        "c-1",
    )
    .expect("200 consumes");
    assert_eq!(reply.outcome(), ConsumeOutcome::Reset { windows_reset: 2 });
    list_reset_credits_at(&urls.list, "at.secret", Some("  "), false).expect("200 lists");

    let seen = handle.join().expect("join stub");
    assert_eq!(seen.len(), 3, "one request per call, no retries");
    let (get, post, blank) = (&seen[0], &seen[1], &seen[2]);
    assert!(
        get.starts_with("GET /backend-api/wham/rate-limit-reset-credits "),
        "{get}"
    );
    assert!(
        post.starts_with("POST /backend-api/wham/rate-limit-reset-credits/consume "),
        "{post}"
    );
    for raw in [get, post] {
        let header = |name| crate::testutil::request_header(raw, name);
        assert_eq!(header("authorization").as_deref(), Some("Bearer at.secret"));
        assert_eq!(header("chatgpt-account-id").as_deref(), Some("acc-1"));
        let ua = header("user-agent").expect("a UA");
        assert!(
            ua == "codex_cli_rs" || ua.starts_with("codex_cli_rs/"),
            "codex's own UA shape: {ua}"
        );
    }
    assert_eq!(
        crate::testutil::request_header(get, "accept"),
        None,
        "codex's backend client sends no Accept on the GET"
    );
    assert_eq!(
        crate::testutil::request_header(post, "accept"),
        None,
        "ureq's default Accept is suppressed on the POST too"
    );
    assert_eq!(
        crate::testutil::request_header(get, "x-openai-fedramp").as_deref(),
        Some("true"),
        "a fedramp account flags itself the way codex does"
    );
    assert_eq!(
        crate::testutil::request_header(post, "x-openai-fedramp"),
        None,
        "an ordinary account sends no fedramp flag"
    );
    assert_eq!(
        crate::testutil::request_header(post, "content-type").as_deref(),
        Some("application/json")
    );
    let body: serde_json::Value =
        serde_json::from_str(&crate::testutil::request_body(post)).expect("json body");
    assert_eq!(
        body,
        serde_json::json!({
            "redeem_request_id": "11111111-2222-4333-8444-555555555555",
            "credit_id": "c-1",
        })
    );
    assert_eq!(
        crate::testutil::request_header(blank, "chatgpt-account-id"),
        None,
        "a blank account id is no id"
    );
}

/// A 401 is its own error on either call, any other non-2xx is its status, a
/// 200 in the wrong shape is a parse failure, and each call is sent once.
#[test]
fn limit_reset_statuses_map_to_their_errors_without_a_retry() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(4, |_path, i| match i {
        0 => (401, r#"{"detail":"stale"}"#.to_string()),
        1 => (401, r#"{"detail":"stale"}"#.to_string()),
        2 => (503, "busy".to_string()),
        _ => (200, "[]".to_string()),
    });
    let urls = ResetUrls::under(&addr);
    assert_eq!(
        list_reset_credits_at(&urls.list, "at", None, false),
        Err(ResetCallError::Unauthorized)
    );
    assert_eq!(
        consume_reset_credit_at(&urls.consume, "at", None, false, "id", "c"),
        Err(ResetCallError::Unauthorized)
    );
    assert_eq!(
        consume_reset_credit_at(&urls.consume, "at", None, false, "id", "c"),
        Err(ResetCallError::Status(503))
    );
    assert_eq!(
        consume_reset_credit_at(&urls.consume, "at", None, false, "id", "c"),
        Err(ResetCallError::Parse)
    );
    assert_eq!(handle.join().expect("join stub").len(), 4);
}

/// Nothing listening is a transport failure, which the consume reports as
/// unconfirmed. A port that was just released refuses the connect at once.
#[test]
fn limit_reset_transport_failure_is_reported_not_retried() {
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let urls = ResetUrls::under(&format!("http://127.0.0.1:{port}"));
    assert_eq!(
        list_reset_credits_at(&urls.list, "at", None, false),
        Err(ResetCallError::Transport)
    );
    let err = consume_reset_credit_at(&urls.consume, "at", None, false, "id", "c")
        .expect_err("no answer");
    assert_eq!(err, ResetCallError::Transport);
    assert!(
        consume_failure("work", &err, 1_780_741_000_000, false)
            .contains("may or may not have gone through")
    );
}

/// A re-send prompt names the earlier request whose answer never arrived, not
/// a credit pick.
#[test]
fn limit_reset_resend_prompt_names_the_earlier_request() {
    assert_eq!(
        re_send_prompt("work"),
        "clauth: re-send the usage-limit reset on 'work' whose answer never arrived? It cannot \
         be undone."
    );
}

/// `--list`'s pending note names the window's end as a local time, pinned on
/// a fixed stamp so the wiring (which instant reaches the copy) is the claim.
#[test]
fn limit_reset_list_note_points_at_the_rerun() {
    let sent_at_ms = 1_780_741_000_000u64;
    assert_eq!(
        list_pending_note("work", "credit-1", sent_at_ms),
        format!(
            "clauth: a usage-limit reset sent at {} got no answer; run `clauth limit-reset work \
             credit-1` before {} to re-send it.",
            local_at(sent_at_ms),
            resend_deadline(sent_at_ms)
        )
    );
}

/// The refusals of a pick the list cannot spend: an id it does not name
/// (escaped, as the operator typed it), a credit that does not read available
/// (its STATE word), and a fresh record's pin on another credit.
#[test]
fn limit_reset_a_refused_pick_names_why() {
    assert_eq!(
        not_listed_line("work", "c\u{7}x"),
        "'work' has no usage-limit reset with the id \"c\\u{7}x\", so no reset was used; run \
         `clauth limit-reset work` to list its resets"
    );
    let redeemed = credit(
        "r",
        "codex_rate_limits",
        "redeemed",
        "2026-01-01T00:00:00Z",
        None,
    );
    assert_eq!(
        unavailable_credit_line("work", &redeemed),
        "the usage-limit reset \"r\" on 'work' cannot be used now (redeemed), so no reset was used"
    );
    let sent_at_ms = 1_780_741_000_000u64;
    assert_eq!(
        pinned_record_line("work", "credit-1", sent_at_ms),
        format!(
            "a usage-limit reset sent at {} on 'work' got no answer; run `clauth limit-reset \
             work credit-1` before {} to re-send it, no other reset is used until then",
            local_at(sent_at_ms),
            resend_deadline(sent_at_ms)
        )
    );
}

/// The reading a codex list read lands keeps every credit as the list sent
/// it, counts the available ones, and stamps the read; it round-trips through
/// the file.
#[test]
fn limit_reset_a_list_read_lands_every_credit() {
    let credits = list_of(
        vec![
            ResetCredit {
                title: Some("Full reset".to_string()),
                ..credit(
                    "a",
                    "codex_rate_limits",
                    "available",
                    "2026-01-01T00:00:00Z",
                    None,
                )
            },
            credit(
                "r",
                "codex_rate_limits",
                "redeemed",
                "2026-01-01T00:00:00Z",
                None,
            ),
        ],
        5,
    );
    let reading = credits_reading(&credits);
    assert_eq!((reading.usable, reading.banked), (1, Some(1)));
    assert_eq!(reading.credits, credits.credits);
    assert!(reading.grants.is_empty());
    assert!(reading.fetched_at.is_some());
    let text = serde_json::to_string(&reading).expect("serializes");
    let back: crate::usage::LimitResets = serde_json::from_str(&text).expect("loads back");
    assert_eq!(back.credits, credits.credits);
}

/// A consume reply settles the record exactly when its code is a defined one;
/// an unknown code leaves the outcome open.
#[test]
fn limit_reset_consume_reply_is_settled_only_on_a_defined_code() {
    let reply = |code: &str| ConsumeReply {
        code: code.to_string(),
        windows_reset: 0,
    };
    for (code, want) in [
        ("reset", true),
        ("already_redeemed", true),
        ("nothing_to_reset", true),
        ("no_credit", true),
        ("partially_reset", false),
        ("weird", false),
    ] {
        assert_eq!(consume_settled(&reply(code)), want, "{code}");
    }
}

/// A POST's record clears on a fresh send's refusal, on a reply naming a
/// definite outcome, and never on a re-send's failure.
#[test]
fn a_post_settles_only_where_its_outcome_is_known() {
    let reply = |code: &str| ConsumeReply {
        code: code.to_string(),
        windows_reset: 0,
    };
    let definite: Result<ConsumeReply, ResetCallError> = Ok(reply("reset"));
    let unknown: Result<ConsumeReply, ResetCallError> = Ok(reply("weird"));
    let rejected: Result<ConsumeReply, ResetCallError> = Err(ResetCallError::Unauthorized);
    let refused: Result<ConsumeReply, ResetCallError> = Err(ResetCallError::Status(409));
    let busy: Result<ConsumeReply, ResetCallError> = Err(ResetCallError::Status(503));
    let silent: Result<ConsumeReply, ResetCallError> = Err(ResetCallError::Transport);
    let garbled: Result<ConsumeReply, ResetCallError> = Err(ResetCallError::Parse);
    for (reply, want) in [
        (&definite, true),
        (&unknown, false),
        (&rejected, true),
        (&refused, true),
        (&busy, false),
        (&silent, false),
        (&garbled, false),
    ] {
        assert_eq!(
            post_settles(false, reply, consume_settled),
            want,
            "{reply:?}"
        );
    }
    for (reply, want) in [
        (&definite, true),
        (&unknown, false),
        (&rejected, false),
        (&refused, false),
        (&busy, false),
        (&silent, false),
        (&garbled, false),
    ] {
        assert_eq!(
            post_settles(true, reply, consume_settled),
            want,
            "{reply:?}"
        );
    }
}
