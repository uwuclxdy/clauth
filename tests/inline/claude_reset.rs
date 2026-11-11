use super::*;

/// The real `/api/oauth/usage?cedar_ember=1&skip_spend=1` body of an account
/// holding no grant: a Max 20x account at its 7d limit.
const NO_GRANT: &str = include_str!("../fixtures/cedar-ember-no-grant.json");

/// A block built from the reset schema Claude Code reads: two usable grants,
/// the server naming the second one next, and one grant cooling down whose
/// `clears` holds a limit name clauth has no short label for.
const THREE_GRANTS: &str = include_str!("../fixtures/cedar-ember-three-grants.json");

fn status(body: &str) -> ResetStatus {
    read_status(body).expect("a cedar_ember block")
}

fn reply(json: &str) -> SpendReply {
    serde_json::from_str(json).expect("a spend reply")
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

#[test]
fn the_real_no_grant_body_lists_its_ineligible_reason() {
    assert_eq!(
        describe_grants("cl1", &status(NO_GRANT)),
        ["clauth: no limit resets available on 'cl1' (no_grant)."]
    );
    assert!(status(NO_GRANT).next_grant().is_none());
}

/// The count is the resets a spend can use now; the grant the server
/// names next is marked; a limit name clauth has no label for is shown as sent.
#[test]
fn a_grant_list_marks_the_servers_next_grant() {
    assert_eq!(
        describe_grants("cl1", &status(THREE_GRANTS)),
        [
            "clauth: 'cl1' has 2 limit resets available.",
            "    Old reset: 1 available, resets 5h",
            "  * Weekly reset: 1 available, resets 5h and 7d  (used next)",
            "    limit reset: 0/1 available, resets 5h and seven_day_blue, cooldown",
        ]
    );
    assert_eq!(
        status(THREE_GRANTS).next_grant().map(|g| g.id.as_str()),
        Some("g-week"),
        "the server's next_grant_id, never the first usable grant"
    );
}

#[test]
fn a_grant_with_an_end_names_it_after_its_state() {
    let body = THREE_GRANTS.replacen(
        r#""ends_at": null, "clears": ["five_hour", "seven_day"]"#,
        r#""ends_at": "2026-10-30T16:00:00+00:00", "clears": ["five_hour", "seven_day"]"#,
        1,
    );
    let lines = describe_grants("cl1", &status(&body));
    assert!(
        lines[2].starts_with("  * Weekly reset: 1 available, resets 5h and 7d, expires "),
        "{}",
        lines[2]
    );
    assert!(lines[2].ends_with("  (used next)"), "{}", lines[2]);
}

/// Each row names its own grant's end: two grants ending at different
/// instants print two different stamps, so a row never borrows the next
/// grant's end.
#[test]
fn each_grant_row_names_its_own_end() {
    let old_ends = THREE_GRANTS.replacen(
        r#""ends_at": null, "clears": ["five_hour"]"#,
        r#""ends_at": "2027-01-01T00:00:00+00:00", "clears": ["five_hour"]"#,
        1,
    );
    assert_ne!(old_ends, THREE_GRANTS, "the old grant's end applied");
    let body = old_ends.replacen(
        r#""ends_at": null, "clears": ["five_hour", "seven_day"]"#,
        r#""ends_at": "2027-03-09T07:59:00+00:00", "clears": ["five_hour", "seven_day"]"#,
        1,
    );
    assert_ne!(body, old_ends, "the weekly grant's end applied");
    let old = crate::format::local_stamp(1_798_761_600).expect("stamp");
    let week = crate::format::local_stamp(1_804_579_140).expect("stamp");

    assert_eq!(
        describe_grants("cl1", &status(&body)),
        [
            "clauth: 'cl1' has 2 limit resets available.".to_string(),
            format!("    Old reset: 1 available, resets 5h, expires {old}"),
            format!("  * Weekly reset: 1 available, resets 5h and 7d, expires {week}  (used next)"),
            "    limit reset: 0/1 available, resets 5h and seven_day_blue, cooldown".to_string(),
        ]
    );
}

/// `--list` prints the server's label whole; the spend prompt cuts it to 40
/// cells.
#[test]
fn a_long_server_label_is_whole_in_the_list_and_cut_in_the_prompt() {
    let long = "x".repeat(500);
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        &format!(r#""label": "{long}""#),
        1,
    );
    let status = status(&body);
    let prompt = spend_prompt("cl1", &status, status.next_grant().expect("a next grant"));
    let cut = format!("{}...", "x".repeat(37));
    assert!(prompt.contains(&format!("\"{cut}\" on 'cl1'")), "{prompt}");
    let lines = describe_grants("cl1", &status);
    assert_eq!(
        lines[2],
        format!("  * {long}: 1 available, resets 5h and 7d  (used next)")
    );
}

/// A whole label still drops the blanks around it on both surfaces.
#[test]
fn a_padded_server_label_prints_trimmed() {
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        r#""label": "  Weekly reset  ""#,
        1,
    );
    assert_ne!(body, THREE_GRANTS, "the padded label applied");
    let status = status(&body);
    assert_eq!(
        describe_grants("cl1", &status)[2],
        "  * Weekly reset: 1 available, resets 5h and 7d  (used next)"
    );
    let prompt = spend_prompt("cl1", &status, status.next_grant().expect("a next grant"));
    assert!(
        prompt.starts_with("clauth: use the limit reset \"Weekly reset\" on 'cl1'?"),
        "{prompt}"
    );
}

/// The window Fable counts into reads `7d fable` on every reset surface, the
/// name the usage bars and MCP `profiles` give the same window.
#[test]
fn the_fable_weekly_reads_7d_fable() {
    assert_eq!(window_label("seven_day_overage_included"), "7d fable");
    let body = THREE_GRANTS.replacen(
        r#""clears": ["five_hour", "seven_day"]"#,
        r#""clears": ["seven_day_overage_included"]"#,
        1,
    );
    let status = status(&body);
    let prompt = spend_prompt("cl1", &status, status.next_grant().expect("a next grant"));
    assert!(prompt.contains("It resets 7d fable now"), "{prompt}");
    assert_eq!(
        describe_grants("cl1", &status)[2],
        "  * Weekly reset: 1 available, resets 7d fable  (used next)"
    );
}

#[test]
fn the_prompt_names_the_grant_and_what_it_clears() {
    let at_limit = status(THREE_GRANTS);
    let grant = at_limit.next_grant().expect("a next grant");
    assert_eq!(
        spend_prompt("cl1", &at_limit, grant),
        "clauth: use the limit reset \"Weekly reset\" on 'cl1'? It resets 5h and 7d now \
         and cannot be undone; 1 available on it."
    );

    let early = status(
        &THREE_GRANTS
            .replacen(r#""at_limit": true"#, r#""at_limit": false"#, 1)
            .replacen(
                r#""percent_used": {"five_hour": 100, "seven_day": 40}"#,
                r#""percent_used": {"five_hour": 70, "seven_day": 40}"#,
                1,
            ),
    );
    let grant = early.next_grant().expect("a next grant");
    assert_eq!(
        spend_prompt("cl1", &early, grant),
        "clauth: use the limit reset \"Weekly reset\" on 'cl1'? It resets 5h and 7d now \
         and cannot be undone; 1 available on it. 'cl1' still has 60% of its 7d left.",
        "short of a limit, the warning names the cleared window the reset throws the most \
         headroom away on"
    );

    let untitled =
        status(&THREE_GRANTS.replacen(r#""label": "Weekly reset""#, r#""label": """#, 1));
    let grant = untitled.next_grant().expect("a next grant");
    assert_eq!(
        spend_prompt("cl1", &untitled, grant),
        "clauth: use a limit reset on 'cl1'? It resets 5h and 7d now and cannot be undone; \
         1 available on it."
    );
}

#[test]
fn the_prompt_names_the_weekly_reset_it_leaves_in_place() {
    let body = THREE_GRANTS.replacen(
        r#""weekly_resets_at": null"#,
        r#""weekly_resets_at": "2026-10-31T16:00:00+00:00""#,
        1,
    );
    let status = status(&body);
    let prompt = spend_prompt("cl1", &status, status.next_grant().expect("a next grant"));
    assert!(
        prompt.starts_with(
            "clauth: use the limit reset \"Weekly reset\" on 'cl1'? It resets 5h and 7d now \
             and cannot be undone; 1 available on it. The weekly reset stays "
        ),
        "{prompt}"
    );
    assert!(prompt.ends_with('.'), "{prompt}");
}

/// Server text reaches a terminal through both the prompt and `--list`, so its
/// control characters are dropped.
#[test]
fn server_text_reaches_the_terminal_without_control_characters() {
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        r#""label": "Weekly\u001b]52;c;AAAA\u0007 reset""#,
        1,
    );
    let status = status(&body);
    let prompt = spend_prompt("cl1", &status, status.next_grant().expect("a next grant"));
    let lines = describe_grants("cl1", &status);
    for text in std::iter::once(&prompt).chain(lines.iter()) {
        assert!(!text.chars().any(char::is_control), "{text:?}");
    }
    assert!(prompt.contains("\"Weekly]52;c;AAAA reset\""), "{prompt}");
}

#[test]
fn a_body_with_no_reset_block_in_its_shape_is_a_parse_failure() {
    assert!(matches!(
        read_status(r#"{"cedar_ember": {"grants": "none"}}"#),
        Err(ResetCallError::Parse)
    ));
    assert!(matches!(
        read_status("not json"),
        Err(ResetCallError::Parse)
    ));
}

/// Every `result` the spend can answer gets its own line; only `reset` is a
/// success, and a result or reason that leaves the outcome open says so and
/// names the re-send window's end.
#[test]
fn every_spend_result_maps_to_its_own_line() {
    let sent_at_ms = 1_700_000_000_000;
    let hint = format!(
        "a re-run before {} re-sends the same request",
        resend_deadline(sent_at_ms)
    );
    let cases: [(&str, Result<String, String>); 9] = [
        (
            r#"{"result":"reset","reason":null,"resets_left":0,"cleared":["five_hour","seven_day"],"weekly_resets_at":null,"cooldown_until":null}"#,
            Ok("clauth: used a limit reset on 'cl1': reset 5h and 7d, 0 left.".to_string()),
        ),
        (
            r#"{"result":"reset","reason":null,"resets_left":null,"cleared":[],"weekly_resets_at":null,"cooldown_until":null}"#,
            Ok("clauth: used a limit reset on 'cl1'.".to_string()),
        ),
        (
            r#"{"result":"already_used","reason":"already_used","resets_left":0,"cleared":[]}"#,
            Err(
                "that limit reset on 'cl1' was already used; run `clauth limit-reset cl1 --list` \
                 to see what is left"
                    .to_string(),
            ),
        ),
        (
            r#"{"result":"not_limited","reason":"not_limited","resets_left":1,"cleared":[]}"#,
            Err("'cl1' is not at a limit that reset covers, so no reset was used".to_string()),
        ),
        (
            r#"{"result":"cooldown","reason":"cooldown","resets_left":1,"cleared":[],"cooldown_until":null}"#,
            Err("limit resets on 'cl1' are cooling down, so no reset was used".to_string()),
        ),
        (
            r#"{"result":"ineligible","reason":"tier","resets_left":null,"cleared":[]}"#,
            Err("'cl1' cannot use a limit reset (tier), so no reset was used".to_string()),
        ),
        (
            r#"{"result":"unavailable","reason":"not_next_grant","resets_left":1,"cleared":[]}"#,
            Err(
                "that limit reset on 'cl1' is unavailable (not_next_grant), so no reset was used"
                    .to_string(),
            ),
        ),
        (
            r#"{"result":"unavailable","reason":"reset_unconfirmed","resets_left":null,"cleared":[]}"#,
            Err(format!(
                "Anthropic could not confirm the reset on 'cl1' (reset_unconfirmed), so the \
                 outcome is unconfirmed; {hint}"
            )),
        ),
        (
            r#"{"result":"boom","reason":null,"resets_left":null,"cleared":[]}"#,
            Err(format!(
                "Anthropic answered the reset request for 'cl1' with an unrecognized result \
                 \"boom\", so the outcome is unconfirmed; {hint}"
            )),
        ),
    ];
    for (json, want) in cases {
        assert_eq!(
            spend_outcome_line("cl1", &reply(json), sent_at_ms),
            want,
            "{json}"
        );
    }
}

#[test]
fn a_cooldown_with_an_end_names_it() {
    let line = spend_outcome_line(
        "cl1",
        &reply(
            r#"{"result":"cooldown","reason":"cooldown","resets_left":1,"cleared":[],"cooldown_until":"2026-10-30T16:00:00+00:00"}"#,
        ),
        1_700_000_000_000,
    )
    .expect_err("a cooldown spends nothing");
    assert!(
        line.starts_with("limit resets on 'cl1' are cooling down until "),
        "{line}"
    );
    assert!(line.ends_with(", so no reset was used"), "{line}");
}

/// A failed POST leaves the outcome open: a 4xx or 401 proves nothing was
/// spent and keeps the look-before-retrying hint, every other failure ends on
/// the re-run the pending record makes safe, naming the window's end.
#[test]
fn a_failed_spend_says_the_outcome_is_open() {
    let sent_at_ms = 1_780_741_000_000u64;
    let deadline = resend_deadline(sent_at_ms);
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Transport, sent_at_ms, false),
        format!(
            "the reset request for 'cl1' got no answer, so the reset may or may not have gone \
             through; a re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Status(500), sent_at_ms, false),
        format!(
            "Anthropic answered the reset request for 'cl1' with HTTP 500; a re-run before \
             {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Parse, sent_at_ms, false),
        format!(
            "Anthropic answered the reset request for 'cl1' in a shape clauth does not read, so \
             the reset may or may not have gone through; a re-run before {deadline} re-sends the \
             same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Status(409), sent_at_ms, false),
        "Anthropic answered the reset request for 'cl1' with HTTP 409; check \
         `clauth limit-reset cl1 --list` before retrying"
    );
}

/// A re-send's failed POST says the earlier request's outcome is still unknown,
/// whatever refused this one, and names the window's end.
#[test]
fn a_resend_failure_keeps_the_earlier_outcome_open() {
    let sent_at_ms = 1_780_741_000_000u64;
    let deadline = resend_deadline(sent_at_ms);
    let open = "the earlier request's outcome is still unknown";
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Unauthorized, sent_at_ms, true),
        format!(
            "Anthropic rejected the stored access token for 'cl1'; clauth refreshes it at its \
             next poll, and {open}; a re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Status(409), sent_at_ms, true),
        format!(
            "Anthropic answered the re-sent reset request for 'cl1' with HTTP 409; {open}; a \
             re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Status(500), sent_at_ms, true),
        format!(
            "Anthropic answered the re-sent reset request for 'cl1' with HTTP 500; {open}; a \
             re-run before {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Transport, sent_at_ms, true),
        format!(
            "the re-sent reset request for 'cl1' got no answer; {open}; a re-run before \
             {deadline} re-sends the same request"
        )
    );
    assert_eq!(
        spend_failure("cl1", &ResetCallError::Parse, sent_at_ms, true),
        format!(
            "Anthropic answered the re-sent reset request for 'cl1' in a shape clauth does not \
             read; {open}; a re-run before {deadline} re-sends the same request"
        )
    );
}

/// The idempotency key matches the pattern Claude Code validates it against,
/// `^[A-Za-z0-9_-]{1,64}$`, and is fresh per call.
#[test]
fn a_request_id_fits_the_servers_pattern_and_is_fresh() {
    let a = new_request_id().expect("a key");
    let b = new_request_id().expect("a key");
    for key in [&a, &b] {
        assert!(
            (1..=64).contains(&key.len())
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{key}"
        );
    }
    assert_ne!(a, b);
}

/// With no grant named next, `--list` still shows every grant the server
/// sent, under the none-available header, and counts none of them: the spend
/// path refuses this account with that same sentence.
#[test]
fn a_grant_list_with_no_next_grant_still_lists_every_grant() {
    let body = THREE_GRANTS.replacen(
        r#""next_grant_id": "g-week""#,
        r#""next_grant_id": null"#,
        1,
    );
    assert_eq!(
        describe_grants("cl1", &status(&body)),
        [
            "clauth: no limit resets available on 'cl1'.",
            "    Old reset: 0/1 available, resets 5h",
            "    Weekly reset: 0/1 available, resets 5h and 7d",
            "    limit reset: 0/1 available, resets 5h and seven_day_blue, cooldown",
        ]
    );
    assert_eq!(status(&body).usable_resets(), 0);
}

/// A next id naming no listed grant is no next grant: the spend path refuses,
/// and the count reads 0 whatever grants beside it are usable.
#[test]
fn a_next_grant_id_naming_no_listed_grant_counts_nothing() {
    let body = THREE_GRANTS.replacen(
        r#""next_grant_id": "g-week""#,
        r#""next_grant_id": "g-gone""#,
        1,
    );
    assert_eq!(
        describe_grants("cl1", &status(&body)),
        [
            "clauth: no limit resets available on 'cl1'.",
            "    Old reset: 0/1 available, resets 5h",
            "    Weekly reset: 0/1 available, resets 5h and 7d",
            "    limit reset: 0/1 available, resets 5h and seven_day_blue, cooldown",
        ]
    );
    assert_eq!(status(&body).usable_resets(), 0);
}

/// The server spends only its next grant, so a next grant in cooldown refuses
/// the spend whatever else is usable: the count reads 0 and no row counts
/// one, the same answer the spend path gives.
#[test]
fn a_next_grant_the_server_will_not_spend_counts_nothing() {
    let body = THREE_GRANTS.replacen(
        r#""next_grant_id": "g-week""#,
        r#""next_grant_id": "g-cool""#,
        1,
    );
    assert_eq!(
        describe_grants("cl1", &status(&body)),
        [
            "clauth: 'cl1' has 0 limit resets available.",
            "    Old reset: 0/1 available, resets 5h",
            "    Weekly reset: 0/1 available, resets 5h and 7d",
            "  * limit reset: 0/1 available, resets 5h and seven_day_blue, cooldown  \
             (used next)",
        ]
    );
    assert_eq!(status(&body).usable_resets(), 0);
}

/// A next grant whose id clauth refuses to send refuses the spend too, though
/// the server marks it usable.
#[test]
fn a_next_grant_clauth_will_not_send_counts_nothing() {
    let body = THREE_GRANTS
        .replacen(r#""id": "g-week""#, r#""id": "G WEEK""#, 1)
        .replacen(
            r#""next_grant_id": "g-week""#,
            r#""next_grant_id": "G WEEK""#,
            1,
        );
    assert_eq!(
        describe_grants("cl1", &status(&body)),
        [
            "clauth: 'cl1' has 0 limit resets available.",
            "    Old reset: 0/1 available, resets 5h",
            "  * Weekly reset: 0/1 available, resets 5h and 7d  (used next)",
            "    limit reset: 0/1 available, resets 5h and seven_day_blue, cooldown",
        ]
    );
    assert_eq!(status(&body).usable_resets(), 0);
}

/// A reset read whose block is null or absent reads as no grant, never as a
/// parse failure.
#[test]
fn a_null_or_absent_reset_block_reads_as_no_grant() {
    for body in [r#"{"cedar_ember": null}"#, r#"{"five_hour": null}"#] {
        assert_eq!(
            describe_grants("cl1", &status(body)),
            ["clauth: no limit resets available on 'cl1'."],
            "{body}"
        );
        assert!(status(body).next_grant().is_none(), "{body}");
    }
}

/// Server text reaches the `[y/N]` line and `--list` without format
/// characters (a right-to-left override would reorder the confirm line), and
/// a server reason cut to a bounded length.
#[test]
fn server_text_drops_format_characters_and_is_bounded() {
    let long = "x".repeat(500);
    let body = THREE_GRANTS
        .replacen(
            r#""label": "Weekly reset""#,
            "\"label\": \"Weekly\u{202e} reset\u{200b}\"",
            1,
        )
        .replacen(
            r#""unavailable_reason": "cooldown""#,
            &format!(r#""unavailable_reason": "{long}""#),
            1,
        );
    let status = status(&body);
    let prompt = spend_prompt("cl1", &status, status.next_grant().expect("a next grant"));
    let lines = describe_grants("cl1", &status);
    for text in std::iter::once(&prompt).chain(lines.iter()) {
        assert!(
            !text.contains('\u{202e}'),
            "bidi characters dropped: {text:?}"
        );
        assert!(
            !text.contains(&"x".repeat(41)),
            "a server reason is cut to 40 cells: {text}"
        );
    }
    assert!(prompt.contains("\"Weekly reset\u{200b}\""), "{prompt}");
}

/// A reason that says the server could not settle the outcome is never read
/// as nothing spent.
#[test]
fn an_indeterminate_spend_reason_leaves_the_outcome_open() {
    let sent_at_ms = 1_700_000_000_000;
    let line = spend_outcome_line(
        "cl1",
        &reply(r#"{"result":"unavailable","reason":"stamp_indeterminate","resets_left":null,"cleared":[]}"#),
        sent_at_ms,
    )
    .expect_err("not a reset");
    assert_eq!(
        line,
        format!(
            "Anthropic could not confirm the reset on 'cl1' (stamp_indeterminate), so the \
             outcome is unconfirmed; a re-run before {} re-sends the same request",
            resend_deadline(sent_at_ms)
        )
    );
}

/// A re-send prompt names the earlier request whose answer never arrived, not
/// a grant pick.
#[test]
fn the_resend_prompt_names_the_earlier_request() {
    assert_eq!(
        re_send_prompt("cl1"),
        "clauth: re-send the limit reset on 'cl1' whose answer never arrived? It cannot be \
         undone."
    );
}

/// `--list`'s pending note names the window's end as a local time, pinned on
/// a fixed stamp so the wiring (which instant reaches the copy) is the claim.
#[test]
fn the_list_note_points_at_the_rerun() {
    let sent_at_ms = 1_780_741_000_000u64;
    assert_eq!(
        list_pending_note("cl1", sent_at_ms),
        format!(
            "clauth: a limit reset sent at {} got no answer; run `clauth limit-reset cl1` \
             again before {} to re-send it.",
            local_at(sent_at_ms),
            resend_deadline(sent_at_ms)
        )
    );
}

/// A reason made only of zero-width characters is no reason: it never prints
/// as an invisible parenthetical.
#[test]
fn a_zero_width_ineligible_reason_reads_as_no_reason() {
    let status = ResetStatus {
        ineligible_reason: Some("\u{200b}".to_string()),
        ..ResetStatus::default()
    };
    assert_eq!(
        no_resets_available("cl1", &status),
        "no limit resets available on 'cl1'"
    );
}

/// A server reason that a terminal draws as nothing is no reason at every
/// claude-arm reason site: the grant state, the `ineligible` refusal and the
/// `unavailable` refusal never print an empty parenthetical.
#[test]
fn a_blank_server_reason_reads_as_no_reason() {
    let status = ResetStatus {
        grants: vec![Grant {
            id: "g".to_string(),
            label: String::new(),
            resets_left: 1,
            ends_at: None,
            clears: vec!["five_hour".to_string(), "seven_day_blue".to_string()],
            paused: false,
            usable_now: false,
            percent_used: std::collections::HashMap::new(),
            unavailable_reason: Some("\u{200b}".to_string()),
        }],
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: no limit resets available on 'cl1'.",
            "    limit reset: 0/1 available, resets 5h and seven_day_blue",
        ]
    );
    assert_eq!(
        spend_outcome_line(
            "cl1",
            &SpendReply {
                result: "ineligible".to_string(),
                reason: Some("\u{200b}".to_string()),
                resets_left: None,
                cleared: vec![],
                cooldown_until: None,
            },
            1_700_000_000_000,
        ),
        Err("'cl1' cannot use a limit reset, so no reset was used".to_string())
    );
    assert_eq!(
        spend_outcome_line(
            "cl1",
            &SpendReply {
                result: "unavailable".to_string(),
                reason: Some("\u{200b}".to_string()),
                resets_left: None,
                cleared: vec![],
                cooldown_until: None,
            },
            1_700_000_000_000,
        ),
        Err("that limit reset on 'cl1' is unavailable, so no reset was used".to_string())
    );
}

/// A stamp that a terminal draws as nothing is no stamp at the three stamp
/// sites: a grant's `ends_at`, a cooldown's `until`, and the prompt's weekly
/// reset stay, so no empty `, ends` / ` until` / `stays` clause prints.
#[test]
fn a_blank_stamp_reads_as_no_stamp() {
    let next = Grant {
        id: "g".to_string(),
        label: String::new(),
        resets_left: 1,
        ends_at: Some("\u{200b}".to_string()),
        clears: vec!["five_hour".to_string()],
        paused: false,
        usable_now: true,
        percent_used: std::collections::HashMap::new(),
        unavailable_reason: None,
    };
    let status = ResetStatus {
        grants: vec![next],
        next_grant_id: Some("g".to_string()),
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: 'cl1' has 1 limit reset available.",
            "  * limit reset: 1 available, resets 5h  (used next)",
        ]
    );

    assert_eq!(
        spend_outcome_line(
            "cl1",
            &SpendReply {
                result: "cooldown".to_string(),
                reason: Some("cooldown".to_string()),
                resets_left: Some(1),
                cleared: vec![],
                cooldown_until: Some("\u{200b}".to_string()),
            },
            1_700_000_000_000,
        ),
        Err("limit resets on 'cl1' are cooling down, so no reset was used".to_string())
    );

    let weekly = ResetStatus {
        grants: vec![Grant {
            id: "g".to_string(),
            label: String::new(),
            resets_left: 1,
            ends_at: None,
            clears: vec!["five_hour".to_string()],
            paused: false,
            usable_now: true,
            percent_used: std::collections::HashMap::new(),
            unavailable_reason: None,
        }],
        next_grant_id: Some("g".to_string()),
        weekly_resets_at: Some("\u{200b}".to_string()),
        ..ResetStatus::default()
    };
    let prompt = spend_prompt("cl1", &weekly, weekly.next_grant().expect("a next grant"));
    assert!(
        !prompt.contains("weekly reset stays"),
        "a blank weekly stamp prints no clause: {prompt}"
    );
    assert!(
        prompt.ends_with("cannot be undone; 1 available on it."),
        "a grant with every reset left reads its bare count: {prompt}"
    );
}

/// The Fable weekly window a grant clears reads as the usage bars name it,
/// in the list row and in the spend prompt.
#[test]
fn a_fable_window_grant_reads_7d_fable() {
    let status = ResetStatus {
        grants: vec![Grant {
            id: "g".to_string(),
            label: "Fable reset".to_string(),
            resets_left: 1,
            ends_at: None,
            clears: vec!["seven_day_overage_included".to_string()],
            paused: false,
            usable_now: true,
            percent_used: std::collections::HashMap::new(),
            unavailable_reason: None,
        }],
        next_grant_id: Some("g".to_string()),
        at_limit: true,
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: 'cl1' has 1 limit reset available.",
            "  * Fable reset: 1 available, resets 7d fable  (used next)",
        ]
    );
    assert_eq!(
        spend_prompt("cl1", &status, status.next_grant().expect("a next grant")),
        "clauth: use the limit reset \"Fable reset\" on 'cl1'? It resets 7d fable now and \
         cannot be undone; 1 available on it."
    );
}

/// A reply names a definite outcome exactly when its result is a defined one
/// and its reason is not the indeterminate one; everything else stays open.
#[test]
fn a_spend_reply_is_settled_only_on_a_defined_result() {
    for (json, want) in [
        (r#"{"result":"reset","reason":null}"#, true),
        (r#"{"result":"already_used","reason":"already_used"}"#, true),
        (r#"{"result":"not_limited","reason":"not_limited"}"#, true),
        (r#"{"result":"cooldown","reason":"cooldown"}"#, true),
        (r#"{"result":"ineligible","reason":"tier"}"#, true),
        (
            r#"{"result":"unavailable","reason":"not_next_grant"}"#,
            true,
        ),
        (r#"{"result":"reset","reason":"reset_unconfirmed"}"#, false),
        (
            r#"{"result":"unavailable","reason":"stamp_indeterminate"}"#,
            false,
        ),
        (r#"{"result":"boom","reason":null}"#, false),
    ] {
        assert_eq!(spend_settled(&reply(json)), want, "{json}");
    }
}

/// A server reason is capped by drawn cells: a 2-cell glyph spends two of the
/// 37 cells the 40-cell cap leaves beside its `...`, so 18 of them fit.
#[test]
fn a_server_reason_cap_counts_wide_glyphs_by_cells() {
    let reason = "漢".repeat(70);
    let json =
        format!(r#"{{"result":"unavailable","reason":"{reason}","resets_left":1,"cleared":[]}}"#);
    assert_eq!(
        spend_outcome_line("cl1", &reply(&json), 1_700_000_000_000),
        Err(format!(
            "that limit reset on 'cl1' is unavailable ({}...), so no reset was used",
            "漢".repeat(18)
        ))
    );
}

/// An unrecognized result is escaped before it is cut, as the codex arm's
/// unrecognized code is: the two-cell `\n` escape counts against the 37-cell
/// content budget, so 30 x's leave room for 5 y's, and `...` sits outside the
/// quotes.
#[test]
fn an_unrecognized_result_is_cut_after_it_is_escaped() {
    let sent_at_ms = 1_700_000_000_000;
    let json = format!(
        r#"{{"result":"{}\n{}","reason":null,"resets_left":null,"cleared":[]}}"#,
        "x".repeat(30),
        "y".repeat(20)
    );
    assert_eq!(
        spend_outcome_line("cl1", &reply(&json), sent_at_ms),
        Err(format!(
            "Anthropic answered the reset request for 'cl1' with an unrecognized result \
             \"{}\\n{}\"..., so the outcome is unconfirmed; a re-run before {} re-sends the \
             same request",
            "x".repeat(30),
            "y".repeat(5),
            resend_deadline(sent_at_ms)
        ))
    );
}

/// Usable counts summing past `u32::MAX` read as maxed, never wrapping (a
/// release build) or overflowing (a debug-build panic) — the codex arm clamps
/// its server count the same way. A grant not usable now contributes nothing.
#[test]
fn usable_resets_saturates_instead_of_overflowing() {
    let grant = |id: &str, left: u32, usable_now: bool| Grant {
        id: id.to_string(),
        label: String::new(),
        resets_left: left,
        ends_at: None,
        clears: vec![],
        paused: false,
        usable_now,
        percent_used: std::collections::HashMap::new(),
        unavailable_reason: None,
    };
    let status = ResetStatus {
        grants: vec![
            grant("g1", u32::MAX, true),
            grant("g2", u32::MAX, true),
            grant("g3", u32::MAX, false),
        ],
        next_grant_id: Some("g1".to_string()),
        ..ResetStatus::default()
    };
    assert_eq!(status.usable_resets(), u32::MAX);
    let just_one = ResetStatus {
        grants: vec![grant("g1", u32::MAX, true)],
        next_grant_id: Some("g1".to_string()),
        ..ResetStatus::default()
    };
    assert_eq!(
        just_one.usable_resets(),
        u32::MAX,
        "one maxed grant is maxed"
    );
}

/// The banked total is every grant's resets left, usable or not, and it
/// saturates: the Usage row's denominator must count a cooling grant and never
/// wrap.
#[test]
fn banked_resets_counts_unusable_grants_and_saturates() {
    let grant = |id: &str, left: u32, usable_now: bool| Grant {
        id: id.to_string(),
        label: String::new(),
        resets_left: left,
        ends_at: None,
        clears: vec![],
        paused: false,
        usable_now,
        percent_used: std::collections::HashMap::new(),
        unavailable_reason: None,
    };
    let status = ResetStatus {
        grants: vec![
            grant("g1", 1, true),
            grant("g2", 2, false),
            grant("g3", 3, false),
        ],
        ..ResetStatus::default()
    };
    assert_eq!(
        status.banked_resets(),
        6,
        "a grant in cooldown still counts"
    );

    let maxed = ResetStatus {
        grants: vec![grant("g1", u32::MAX, true), grant("g2", 1, false)],
        ..ResetStatus::default()
    };
    assert_eq!(maxed.banked_resets(), u32::MAX, "the sum saturates");
}

/// `--list`'s count line reads the same saturated count the side read lands:
/// two usable grants at `u32::MAX` say `4294967295`, never a wrapped sum or an
/// overflow panic, and the grant that is not usable now adds nothing.
#[test]
fn the_list_count_line_saturates_like_the_side_read() {
    let grant = |id: &str, left: u32, usable_now: bool| Grant {
        id: id.to_string(),
        label: String::new(),
        resets_left: left,
        ends_at: None,
        clears: vec![],
        paused: false,
        usable_now,
        percent_used: std::collections::HashMap::new(),
        unavailable_reason: None,
    };
    let status = ResetStatus {
        grants: vec![
            grant("g1", u32::MAX, true),
            grant("g2", u32::MAX, true),
            grant("g3", 7, false),
        ],
        next_grant_id: Some("g1".to_string()),
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status)[0],
        "clauth: 'cl1' has 4294967295 limit resets available."
    );
}

/// Only a reply that actually took a reset off the count moves it: the spend
/// landed, or the grant was already gone. Every refusal, open outcome and
/// unrecognized result spends nothing, so the post-spend refresh re-reads
/// only on the first two.
#[test]
fn only_a_landed_or_already_used_reply_moves_the_count() {
    for (json, want) in [
        (r#"{"result":"reset","reason":null}"#, true),
        (r#"{"result":"already_used","reason":"already_used"}"#, true),
        (r#"{"result":"not_limited","reason":"not_limited"}"#, false),
        (r#"{"result":"cooldown","reason":"cooldown"}"#, false),
        (r#"{"result":"ineligible","reason":"tier"}"#, false),
        (
            r#"{"result":"unavailable","reason":"not_next_grant"}"#,
            false,
        ),
        (r#"{"result":"reset","reason":"reset_unconfirmed"}"#, false),
        (
            r#"{"result":"unavailable","reason":"stamp_indeterminate"}"#,
            false,
        ),
        (r#"{"result":"boom","reason":null}"#, false),
    ] {
        assert_eq!(spend_moved_the_count(&reply(json)), want, "{json}");
    }
}

/// The reading the hourly side read and the post-spend refresh both land: the
/// usable count, the banked total, the next grant's raw window names and end,
/// and the read's own stamp, so a spend's re-read writes exactly what the
/// hourly read would have.
#[test]
fn a_status_lands_the_reading_the_side_read_and_the_refresh_share() {
    let before = crate::usage::now_ms();
    let reading = status_reading(&status(THREE_GRANTS));
    let after = crate::usage::now_ms();
    assert_eq!(reading.usable, 2);
    assert_eq!(reading.banked, Some(3), "every grant in the body, one each");
    assert_eq!(reading.clears, ["five_hour", "seven_day"]);
    assert_eq!(reading.ends_at, None, "the next grant has no end");
    let stamp = reading.fetched_at.expect("the read's own stamp");
    assert!(
        (before..=after).contains(&stamp),
        "{before} <= {stamp} <= {after}"
    );

    let ended = THREE_GRANTS.replacen(
        r#""ends_at": null, "clears": ["five_hour", "seven_day"]"#,
        r#""ends_at": "2026-10-30T16:00:00+00:00", "clears": ["five_hour", "seven_day"]"#,
        1,
    );
    let reading = status_reading(&status(&ended));
    assert_eq!(
        reading.ends_at.as_deref(),
        Some("2026-10-30T16:00:00+00:00"),
        "the next grant's end, raw server spelling"
    );

    let none_next = THREE_GRANTS.replacen(
        r#""next_grant_id": "g-week""#,
        r#""next_grant_id": null"#,
        1,
    );
    let reading = status_reading(&status(&none_next));
    assert_eq!(reading.usable, 0, "no next grant, nothing a spend can use");
    assert!(reading.clears.is_empty(), "no next grant, no windows");
    assert_eq!(reading.ends_at, None);
}

/// The failed-re-read line names the read's failure and only what the refresh
/// did with the stored count under the gate: dropped, left alone by the
/// login gate or a state flock, kept as newer than the spend (the next test),
/// or (with the drop-failure line after it) not removed.
#[test]
fn the_refresh_read_failure_line_says_what_became_of_the_stored_count() {
    let err = ResetCallError::Status(503);
    assert_eq!(
        refresh_read_failure("cl2", &err, RefreshDrop::Dropped),
        "clauth: 'cl2': the limit reset read after the spend failed (Anthropic answered with \
         HTTP 503); the stored count was dropped"
    );
    assert_eq!(
        refresh_read_failure("cl2", &err, RefreshDrop::LoginChanged),
        "clauth: 'cl2': the limit reset read after the spend failed (Anthropic answered with \
         HTTP 503); the stored login changed or could not be read, so nothing was touched"
    );
    assert_eq!(
        refresh_read_failure("cl2", &err, RefreshDrop::LockTimedOut),
        "clauth: 'cl2': the limit reset read after the spend failed (Anthropic answered with \
         HTTP 503); the state lock was unavailable, so nothing was touched"
    );
    assert_eq!(
        refresh_read_failure("cl2", &err, RefreshDrop::Failed),
        "clauth: 'cl2': the limit reset read after the spend failed (Anthropic answered with \
         HTTP 503)"
    );
}

/// A stored count read at or after this run's send was kept, and the line
/// says so.
#[test]
fn the_refresh_read_failure_line_keeps_a_newer_stored_count() {
    let err = ResetCallError::Status(503);
    assert_eq!(
        refresh_read_failure("cl2", &err, RefreshDrop::KeptNewer),
        "clauth: 'cl2': the limit reset read after the spend failed (Anthropic answered with \
         HTTP 503); the stored count is newer than the spend, so it stays"
    );
}

/// The two lines for a store that did not land: a write that failed, and a
/// state flock the refresh could not take.
#[test]
fn the_refresh_store_failure_lines_name_what_was_not_stored() {
    assert_eq!(
        refresh_store_failure("cl2"),
        "clauth: 'cl2': the limit reset count read after the spend was not stored"
    );
    assert_eq!(
        refresh_lock_failure("cl2"),
        "clauth: 'cl2': the state lock was unavailable, so the limit reset count read after the \
         spend was not stored"
    );
    assert_eq!(
        refresh_drop_failure("cl2"),
        "clauth: 'cl2': could not remove the stored limit reset count"
    );
}

/// A usable next grant on the pattern is the one the spend sends, and the
/// count's view of the rule names the same grant.
#[test]
fn the_spend_rule_sends_the_servers_usable_next_grant() {
    let status = status(THREE_GRANTS);
    assert_eq!(
        status.try_spend_grant().map(|g| g.id.as_str()).ok(),
        Some("g-week")
    );
    assert_eq!(status.spend_grant().map(|g| g.id.as_str()), Some("g-week"));
}

/// No next grant and a next id naming no listed grant both refuse with the
/// status the no-grant line reads.
#[test]
fn the_spend_rule_refuses_no_next_grant_with_its_status() {
    for next in ["null", r#""g-gone""#] {
        let status = status(&THREE_GRANTS.replacen(
            r#""next_grant_id": "g-week""#,
            &format!(r#""next_grant_id": {next}"#),
            1,
        ));
        match status.try_spend_grant() {
            Err(SpendRefusal::NoNextGrant(carried)) => {
                assert!(std::ptr::eq(carried, &status), "next {next}");
            }
            other => panic!("next {next}: {other:?}"),
        }
        assert!(status.spend_grant().is_none(), "next {next}");
    }
}

/// A next grant in cooldown refuses with that grant, though other grants are
/// usable.
#[test]
fn the_spend_rule_refuses_a_cooling_next_grant_with_the_grant() {
    let status = status(&THREE_GRANTS.replacen(
        r#""next_grant_id": "g-week""#,
        r#""next_grant_id": "g-cool""#,
        1,
    ));
    match status.try_spend_grant() {
        Err(SpendRefusal::NotUsableNow(grant)) => assert_eq!(grant.id, "g-cool"),
        other => panic!("{other:?}"),
    }
    assert!(status.spend_grant().is_none());
}

/// A usable next grant whose id is off the pattern refuses as unsendable.
#[test]
fn the_spend_rule_refuses_a_next_grant_id_off_the_pattern() {
    let status = status(
        &THREE_GRANTS
            .replacen(r#""id": "g-week""#, r#""id": "G WEEK""#, 1)
            .replacen(
                r#""next_grant_id": "g-week""#,
                r#""next_grant_id": "G WEEK""#,
                1,
            ),
    );
    assert!(matches!(
        status.try_spend_grant(),
        Err(SpendRefusal::UnsendableId)
    ));
    assert!(status.spend_grant().is_none());
}

/// A next grant both cooling and off the pattern refuses as not usable now,
/// so the spend names the cooldown rather than the id.
#[test]
fn the_spend_rule_names_a_cooldown_before_an_unsendable_id() {
    let status = status(
        &THREE_GRANTS
            .replacen(r#""id": "g-cool""#, r#""id": "G COOL""#, 1)
            .replacen(
                r#""next_grant_id": "g-week""#,
                r#""next_grant_id": "G COOL""#,
                1,
            ),
    );
    match status.try_spend_grant() {
        Err(SpendRefusal::NotUsableNow(grant)) => assert_eq!(grant.id, "G COOL"),
        other => panic!("{other:?}"),
    }
}
