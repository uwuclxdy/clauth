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

/// The grant the server lists under `id`.
fn grant<'a>(status: &'a ResetStatus, id: &str) -> &'a Grant {
    status
        .grants
        .iter()
        .find(|g| g.id == id)
        .expect("a listed grant")
}

/// The table [`THREE_GRANTS`] lists under its count line and blank line,
/// padded by hand: each column to its widest cell, header included, two
/// spaces between; `g-cool` reads its server reason, `cooldown`, raw.
const THREE_GRANTS_TABLE: [&str; 4] = [
    "  ID      LEFT  STATE     RESETS              EXPIRES  LABEL",
    "  g-old   1/1   ready     5h                  -        Old reset",
    "  g-week  1/2   ready     5h, 7d              -        Weekly reset",
    "  g-cool  1/1   cooldown  5h, seven_day_blue  -        limit reset",
];

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
    assert!(status(NO_GRANT).grants.is_empty());
}

/// The count is the resets a spend can use now against the ones held; the
/// table lists every grant with no grant marked, and a limit name clauth has
/// no label for is shown as sent.
#[test]
fn a_grant_list_is_a_table_marking_no_grant() {
    let mut want = vec![
        "clauth: 'cl1' has 2/3 limit resets available.".to_string(),
        String::new(),
    ];
    want.extend(THREE_GRANTS_TABLE.map(str::to_string));
    assert_eq!(describe_grants("cl1", &status(THREE_GRANTS)), want);
}

/// A grant with an end names it in its own EXPIRES cell, which widens the
/// column for every row.
#[test]
fn a_grant_with_an_end_names_it_in_its_expires_cell() {
    let body = THREE_GRANTS.replacen(
        r#""ends_at": null, "clears": ["five_hour", "seven_day"]"#,
        r#""ends_at": "2026-10-30T16:00:00+00:00", "clears": ["five_hour", "seven_day"]"#,
        1,
    );
    assert_ne!(body, THREE_GRANTS, "the weekly grant's end applied");
    let ends = crate::format::local_minute_stamp(1_793_376_000).expect("stamp");
    let lines = describe_grants("cl1", &status(&body));
    assert_eq!(
        lines[4],
        format!("  g-week  1/2   ready     5h, 7d              {ends}  Weekly reset")
    );
    assert_eq!(
        lines[3],
        format!(
            "  g-old   1/1   ready     5h                  -{}  Old reset",
            " ".repeat(ends.chars().count() - 1)
        )
    );
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
    let old = crate::format::local_minute_stamp(1_798_761_600).expect("stamp");
    let week = crate::format::local_minute_stamp(1_804_579_140).expect("stamp");
    let none = format!("-{}", " ".repeat(old.chars().count() - 1));

    assert_eq!(
        describe_grants("cl1", &status(&body))[2..],
        [
            format!(
                "  ID      LEFT  STATE     RESETS              {:<w$}  LABEL",
                "EXPIRES",
                w = old.chars().count()
            ),
            format!("  g-old   1/1   ready     5h                  {old}  Old reset"),
            format!("  g-week  1/2   ready     5h, 7d              {week}  Weekly reset"),
            format!("  g-cool  1/1   cooldown  5h, seven_day_blue  {none}  limit reset"),
        ]
    );
}

/// The table cuts the server's label to 44 cells in its last column; the
/// spend prompt cuts it to 40 cells.
#[test]
fn a_long_server_label_is_cut_in_the_table_and_in_the_prompt() {
    let long = "x".repeat(500);
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        &format!(r#""label": "{long}""#),
        1,
    );
    let reading = status(&body);
    let prompt = spend_prompt("cl1", &reading, grant(&reading, "g-week"));
    let cut = format!("{}...", "x".repeat(37));
    assert!(prompt.contains(&format!("\"{cut}\" on 'cl1'")), "{prompt}");
    let lines = describe_grants("cl1", &reading);
    assert_eq!(
        lines[4],
        format!(
            "  g-week  1/2   ready     5h, 7d              -        {}...",
            "x".repeat(41)
        )
    );

    // A wide glyph spends two cells: 30 of them are 60, and the 41-cell
    // content budget inside the 44-cell cap keeps 20 whole glyphs.
    let wide = "漢".repeat(30);
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        &format!(r#""label": "{wide}""#),
        1,
    );
    assert_ne!(body, THREE_GRANTS, "the wide label applied");
    let wide_status = status(&body);
    assert_eq!(
        describe_grants("cl1", &wide_status)[4],
        format!(
            "  g-week  1/2   ready     5h, 7d              -        {}...",
            "漢".repeat(20)
        )
    );

    // The boundary is drawn cells: 44 print whole, 45 cut to 41 and `...`.
    let at_cap = "y".repeat(44);
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        &format!(r#""label": "{at_cap}""#),
        1,
    );
    assert_ne!(body, THREE_GRANTS, "the 44-cell label applied");
    let at_cap_status = status(&body);
    assert_eq!(
        describe_grants("cl1", &at_cap_status)[4],
        format!("  g-week  1/2   ready     5h, 7d              -        {at_cap}")
    );

    let over = "z".repeat(45);
    let body = THREE_GRANTS.replacen(
        r#""label": "Weekly reset""#,
        &format!(r#""label": "{over}""#),
        1,
    );
    assert_ne!(body, THREE_GRANTS, "the 45-cell label applied");
    let over_status = status(&body);
    assert_eq!(
        describe_grants("cl1", &over_status)[4],
        format!(
            "  g-week  1/2   ready     5h, 7d              -        {}...",
            "z".repeat(41)
        )
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
    assert_eq!(describe_grants("cl1", &status)[4], THREE_GRANTS_TABLE[2]);
    let prompt = spend_prompt("cl1", &status, grant(&status, "g-week"));
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
    let prompt = spend_prompt("cl1", &status, grant(&status, "g-week"));
    assert!(prompt.contains("It resets 7d fable now"), "{prompt}");
    assert_eq!(
        describe_grants("cl1", &status)[4],
        "  g-week  1/2   ready     7d fable            -        Weekly reset"
    );
}

#[test]
fn the_prompt_names_the_grant_and_what_it_clears() {
    let at_limit = status(THREE_GRANTS);
    assert_eq!(
        spend_prompt("cl1", &at_limit, grant(&at_limit, "g-week")),
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
    assert_eq!(
        spend_prompt("cl1", &early, grant(&early, "g-week")),
        "clauth: use the limit reset \"Weekly reset\" on 'cl1'? It resets 5h and 7d now \
         and cannot be undone; 1 available on it. 'cl1' still has 60% of its 7d left.",
        "short of a limit, the warning names the cleared window the reset throws the most \
         headroom away on"
    );

    let untitled =
        status(&THREE_GRANTS.replacen(r#""label": "Weekly reset""#, r#""label": """#, 1));
    assert_eq!(
        spend_prompt("cl1", &untitled, grant(&untitled, "g-week")),
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
    let prompt = spend_prompt("cl1", &status, grant(&status, "g-week"));
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
    let prompt = spend_prompt("cl1", &status, grant(&status, "g-week"));
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

/// The grant the server would spend next changes nothing: whether it names
/// none, a grant it does not list, or one cooling down, the listing and the
/// count are the ones every usable grant gives.
#[test]
fn the_servers_next_grant_changes_neither_the_listing_nor_the_count() {
    let mut want = vec![
        "clauth: 'cl1' has 2/3 limit resets available.".to_string(),
        String::new(),
    ];
    want.extend(THREE_GRANTS_TABLE.map(str::to_string));
    for next in ["null", r#""g-gone""#, r#""g-cool""#] {
        let body = THREE_GRANTS.replacen(
            r#""next_grant_id": "g-week""#,
            &format!(r#""next_grant_id": {next}"#),
            1,
        );
        assert_ne!(body, THREE_GRANTS, "the next id {next} applied");
        assert_eq!(describe_grants("cl1", &status(&body)), want, "next {next}");
        assert_eq!(status(&body).usable_resets(), 2, "next {next}");
    }
}

/// A grant whose id clauth refuses to send is listed under that id, and the
/// count leaves it out, though the server marks it usable: the count follows
/// the spend rule.
#[test]
fn a_grant_clauth_will_not_send_counts_nothing() {
    let body = THREE_GRANTS.replacen(r#""id": "g-week""#, r#""id": "G WEEK""#, 1);
    assert_ne!(body, THREE_GRANTS, "the id edit applied");
    let lines = describe_grants("cl1", &status(&body));
    assert_eq!(lines[0], "clauth: 'cl1' has 1/3 limit resets available.");
    assert_eq!(
        lines[4], "  G WEEK  1/2   unavailable  5h, 7d              -        Weekly reset",
        "a grant whose id clauth will not send reads unavailable, never ready"
    );
    assert_eq!(status(&body).usable_resets(), 1);
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
        assert!(status(body).grants.is_empty(), "{body}");
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
    let prompt = spend_prompt("cl1", &status, grant(&status, "g-week"));
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
    assert!(
        lines[5].starts_with(&format!("  g-cool  1/1   {}...  ", "x".repeat(17))),
        "the STATE cell cuts a server reason to 20 cells: {}",
        lines[5]
    );
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

/// The listing's pending note names the command that re-sends the recorded
/// grant and the window's end as a local time, pinned on a fixed stamp so the
/// wiring (which instant reaches the copy) is the claim.
#[test]
fn the_list_note_points_at_the_rerun() {
    let sent_at_ms = 1_780_741_000_000u64;
    assert_eq!(
        list_pending_note("cl1", "g-week", sent_at_ms),
        format!(
            "clauth: a limit reset sent at {} got no answer; run `clauth limit-reset cl1 \
             g-week` before {} to re-send it.",
            local_at(sent_at_ms),
            resend_deadline(sent_at_ms)
        )
    );
}

/// A grant every field of which but its id and label is a plain usable one.
fn plain_grant(id: &str, label: &str, clears: &[&str]) -> Grant {
    Grant {
        id: id.to_string(),
        label: label.to_string(),
        resets_total: 1,
        resets_left: 1,
        ends_at: None,
        clears: clears.iter().map(|w| (*w).to_string()).collect(),
        paused: false,
        usable_now: true,
        use_requires_limit: false,
        percent_used: std::collections::HashMap::new(),
        unavailable_reason: None,
    }
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
            usable_now: false,
            unavailable_reason: Some("\u{200b}".to_string()),
            ..plain_grant("g", "", &["five_hour", "seven_day_blue"])
        }],
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: 'cl1' has 0/1 limit reset available.",
            "",
            "  ID  LEFT  STATE        RESETS              EXPIRES  LABEL",
            "  g   1/1   unavailable  5h, seven_day_blue  -        limit reset",
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
    let status = ResetStatus {
        grants: vec![Grant {
            ends_at: Some("\u{200b}".to_string()),
            ..plain_grant("g", "", &["five_hour"])
        }],
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: 'cl1' has 1 limit reset available.",
            "",
            "  ID  LEFT  STATE  RESETS  EXPIRES  LABEL",
            "  g   1/1   ready  5h      -        limit reset",
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
        grants: vec![plain_grant("g", "", &["five_hour"])],
        weekly_resets_at: Some("\u{200b}".to_string()),
        ..ResetStatus::default()
    };
    let prompt = spend_prompt("cl1", &weekly, grant(&weekly, "g"));
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
        grants: vec![plain_grant(
            "g",
            "Fable reset",
            &["seven_day_overage_included"],
        )],
        at_limit: true,
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: 'cl1' has 1 limit reset available.",
            "",
            "  ID  LEFT  STATE  RESETS    EXPIRES  LABEL",
            "  g   1/1   ready  7d fable  -        Fable reset",
        ]
    );
    assert_eq!(
        spend_prompt("cl1", &status, grant(&status, "g")),
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

/// The reason a refusal hands its surface is the reply's own, made
/// terminal-safe and capped like the CLI's reason; a refusal that names none
/// falls back to its result. A reply that landed a reset, one reporting the
/// grant already spent (it moved the count), or one whose outcome stays open
/// names no refusal.
#[test]
fn a_refused_reply_names_the_reason_the_modal_shows() {
    let reason = |json: &str| reply(json).refusal_reason();
    assert_eq!(
        reason(r#"{"result":"unavailable","reason":"not_next_grant"}"#).as_deref(),
        Some("not_next_grant")
    );
    assert_eq!(
        reason(r#"{"result":"cooldown","reason":null}"#).as_deref(),
        Some("cooldown"),
        "a refusal with no reason names its result"
    );
    assert_eq!(
        reason(r#"{"result":"unavailable","reason":"bad\u001b[2Jreason"}"#).as_deref(),
        Some("bad[2Jreason"),
        "the control character never reaches the terminal"
    );
    let capped = format!("{}...", "x".repeat(37));
    assert_eq!(
        reason(&format!(
            r#"{{"result":"unavailable","reason":"{}"}}"#,
            "x".repeat(50)
        ))
        .as_deref(),
        Some(capped.as_str()),
        "the reason is capped at 40 drawn cells"
    );
    for json in [
        r#"{"result":"reset","reason":null}"#,
        r#"{"result":"reset","reason":"reset_unconfirmed"}"#,
        r#"{"result":"already_used","reason":"already_used"}"#,
        r#"{"result":"boom","reason":null}"#,
    ] {
        assert_eq!(reason(json), None, "{json}");
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
        resets_left: left,
        usable_now,
        ..plain_grant(id, "", &[])
    };
    let status = ResetStatus {
        grants: vec![
            grant("g1", u32::MAX, true),
            grant("g2", u32::MAX, true),
            grant("g3", u32::MAX, false),
        ],
        ..ResetStatus::default()
    };
    assert_eq!(status.usable_resets(), u32::MAX);
    let just_one = ResetStatus {
        grants: vec![grant("g1", u32::MAX, true)],
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
        resets_left: left,
        usable_now,
        ..plain_grant(id, "", &[])
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

/// The listing's count line reads the same saturated count the side read
/// lands: two usable grants at `u32::MAX` say `4294967295`, never a wrapped
/// sum or an overflow panic, and the grant that is not usable now adds nothing
/// to it, only to the saturated total it is counted against.
#[test]
fn the_list_count_line_saturates_like_the_side_read() {
    let grant = |id: &str, left: u32, usable_now: bool| Grant {
        resets_left: left,
        usable_now,
        ..plain_grant(id, "", &[])
    };
    let status = ResetStatus {
        grants: vec![
            grant("g1", u32::MAX, true),
            grant("g2", u32::MAX, true),
            grant("g3", 7, false),
        ],
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
/// usable count, the banked total, every grant as the server listed it (each
/// one's own window names and end, raw), the block's cooldown and limit flag,
/// and the read's own stamp, so a spend's re-read writes exactly what the
/// hourly read would have.
#[test]
fn a_status_lands_the_reading_the_side_read_and_the_refresh_share() {
    let before = crate::usage::now_ms();
    let reading = status_reading(&status(THREE_GRANTS));
    let after = crate::usage::now_ms();
    assert_eq!(reading.usable, 2);
    assert_eq!(reading.banked, Some(3), "every grant in the body, one each");
    let ids: Vec<&str> = reading.grants.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(
        ids,
        ["g-old", "g-week", "g-cool"],
        "every grant, server order"
    );
    assert_eq!(reading.grants[1].clears, ["five_hour", "seven_day"]);
    assert_eq!(
        reading.grants[1].ends_at, None,
        "the weekly grant has no end"
    );
    assert!(reading.at_limit);
    assert_eq!(reading.cooldown_until, None);
    let stamp = reading.fetched_at.expect("the read's own stamp");
    assert!(
        (before..=after).contains(&stamp),
        "{before} <= {stamp} <= {after}"
    );

    let ended = THREE_GRANTS
        .replacen(
            r#""ends_at": null, "clears": ["five_hour", "seven_day"]"#,
            r#""ends_at": "2026-10-30T16:00:00+00:00", "clears": ["five_hour", "seven_day"]"#,
            1,
        )
        .replacen(
            r#""cooldown_until": null"#,
            r#""cooldown_until": "2026-10-30T18:00:00+00:00""#,
            1,
        );
    let reading = status_reading(&status(&ended));
    assert_eq!(
        reading.grants[1].ends_at.as_deref(),
        Some("2026-10-30T16:00:00+00:00"),
        "the grant's own end, raw server spelling"
    );
    assert_eq!(
        reading.cooldown_until.as_deref(),
        Some("2026-10-30T18:00:00+00:00"),
        "the block's cooldown, raw server spelling"
    );
}

/// A stored reading keeps what every surface names a reset from, and loads
/// back: a reading an older clauth wrote (a count and the next grant's
/// windows) still loads, carrying no grant.
#[test]
fn a_stored_reading_round_trips_and_an_older_one_still_loads() {
    let reading = status_reading(&status(THREE_GRANTS));
    let text = serde_json::to_string(&reading).expect("serializes");
    let back: crate::usage::LimitResets = serde_json::from_str(&text).expect("loads back");
    assert_eq!(back.usable, 2);
    assert_eq!(back.grants.len(), 3);
    assert_eq!(
        back.grants[2].unavailable_reason.as_deref(),
        Some("cooldown")
    );
    assert_eq!(back.grants[1].resets_total, 2);
    assert!(back.grants[0].use_requires_limit);

    let older: crate::usage::LimitResets = serde_json::from_str(
        r#"{"usable":1,"banked":2,"clears":["five_hour"],"ends_at":"2026-10-30T16:00:00+00:00","fetched_at":1}"#,
    )
    .expect("an older reading still loads");
    assert_eq!((older.usable, older.banked), (1, Some(2)));
    assert!(older.grants.is_empty());
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

/// Every usable grant on the pattern is one a spend sends, whichever the
/// server would spend next, and the count adds up exactly those.
#[test]
fn the_spend_rule_sends_any_usable_grant_its_id_names() {
    let status = status(&THREE_GRANTS.replacen(
        r#""next_grant_id": "g-week""#,
        r#""next_grant_id": "g-cool""#,
        1,
    ));
    for id in ["g-old", "g-week"] {
        assert_eq!(
            status.try_spend_grant_id(id).map(|g| g.id.as_str()).ok(),
            Some(id)
        );
    }
    assert_eq!(status.usable_resets(), 2);
}

/// An id no listed grant carries refuses as not listed, also on a status that
/// lists no grant at all.
#[test]
fn the_spend_rule_refuses_an_id_the_server_does_not_list() {
    for body in [THREE_GRANTS, NO_GRANT] {
        assert!(matches!(
            status(body).try_spend_grant_id("g-gone"),
            Err(SpendRefusal::NotListed)
        ));
    }
}

/// A grant in cooldown refuses with that grant, though other grants are
/// usable.
#[test]
fn the_spend_rule_refuses_a_cooling_grant_with_the_grant() {
    let status = status(THREE_GRANTS);
    match status.try_spend_grant_id("g-cool") {
        Err(SpendRefusal::NotUsableNow(grant)) => assert_eq!(grant.id, "g-cool"),
        other => panic!("{other:?}"),
    }
}

/// A usable grant whose id is off the pattern refuses as unsendable.
#[test]
fn the_spend_rule_refuses_a_grant_id_off_the_pattern() {
    let status = status(&THREE_GRANTS.replacen(r#""id": "g-week""#, r#""id": "G WEEK""#, 1));
    assert!(matches!(
        status.try_spend_grant_id("G WEEK"),
        Err(SpendRefusal::UnsendableId)
    ));
}

/// A grant both cooling and off the pattern refuses as not usable now, so the
/// spend names the cooldown rather than the id.
#[test]
fn the_spend_rule_names_a_cooldown_before_an_unsendable_id() {
    let status = status(&THREE_GRANTS.replacen(r#""id": "g-cool""#, r#""id": "G COOL""#, 1));
    match status.try_spend_grant_id("G COOL") {
        Err(SpendRefusal::NotUsableNow(grant)) => assert_eq!(grant.id, "G COOL"),
        other => panic!("{other:?}"),
    }
}

/// The STATE word a grant reads, in its precedence: `used` once it holds
/// none, `ready` while usable and its id is one clauth sends, `unavailable`
/// for a usable grant whose id is off that pattern, `cooldown <dur>` for any
/// other held grant while the block's cooldown lies ahead, then `paused`, `at
/// limit only` under the limit, the server's own reason, and `unavailable`.
#[test]
fn a_grant_reads_its_state_word() {
    let now = 1_793_376_000;
    let ahead = "2026-10-30T20:46:00+00:00";
    let behind = "2026-10-30T15:00:00+00:00";
    let held = Grant {
        usable_now: false,
        ..plain_grant("g", "", &["five_hour"])
    };
    let cases = [
        (
            Grant {
                resets_left: 0,
                ..held.clone()
            },
            Some(ahead),
            false,
            "used",
        ),
        (plain_grant("g", "", &[]), Some(ahead), false, "ready"),
        (plain_grant("G WEEK", "", &[]), None, false, "unavailable"),
        (
            Grant {
                paused: true,
                ..held.clone()
            },
            Some(ahead),
            false,
            "cooldown 4h 46m",
        ),
        (
            Grant {
                paused: true,
                ..held.clone()
            },
            Some(behind),
            false,
            "paused",
        ),
        (
            Grant {
                use_requires_limit: true,
                ..held.clone()
            },
            None,
            false,
            "at limit only",
        ),
        (
            Grant {
                use_requires_limit: true,
                unavailable_reason: Some("blocked\u{1b}[2J".to_string()),
                ..held.clone()
            },
            None,
            true,
            "blocked[2J",
        ),
        (held, None, false, "unavailable"),
    ];
    for (grant, cooldown, at_limit, want) in cases {
        assert_eq!(
            grant_state(&grant, cooldown, at_limit, now),
            want,
            "{grant:?} cooldown {cooldown:?} at_limit {at_limit}"
        );
    }
}

/// A usable grant whose id is off the pattern clauth sends reads
/// `unavailable` in the table too, so the listing never calls a reset it
/// refuses ready.
#[test]
fn a_grant_off_the_send_pattern_reads_unavailable_in_the_table() {
    let status = ResetStatus {
        grants: vec![plain_grant("G WEEK", "off-pattern reset", &[])],
        ..ResetStatus::default()
    };
    assert_eq!(
        describe_grants("cl1", &status),
        [
            "clauth: 'cl1' has 0/1 limit reset available.",
            "",
            "  ID      LEFT  STATE        RESETS  EXPIRES  LABEL",
            "  G WEEK  1/1   unavailable  -       -        off-pattern reset",
        ]
    );
}

/// The refusals of a pick the read cannot spend: an id it does not list
/// (escaped, as the operator typed it), a listed grant it marks unusable
/// (its STATE word), and a fresh record's pin on another grant.
#[test]
fn a_refused_pick_names_why() {
    assert_eq!(
        not_listed_line("cl1", "g\u{1b}[2Jone"),
        "'cl1' has no limit reset with the id \"g\\u{1b}[2Jone\", so no reset was used; run \
         `clauth limit-reset cl1` to list its resets"
    );
    let status = status(THREE_GRANTS);
    assert_eq!(
        unusable_grant_line("cl1", &status, grant(&status, "g-cool")),
        "the limit reset \"g-cool\" on 'cl1' cannot be used now (cooldown), so no reset was used"
    );
    let sent_at_ms = 1_780_741_000_000u64;
    assert_eq!(
        pinned_record_line("cl1", "g-week", sent_at_ms),
        format!(
            "a limit reset sent at {} on 'cl1' got no answer; run `clauth limit-reset cl1 \
             g-week` before {} to re-send it, no other reset is used until then",
            local_at(sent_at_ms),
            resend_deadline(sent_at_ms)
        )
    );
}

/// A real body holding three usable grants, the server naming the first next.
const THREE_BANKED: &str = include_str!("../fixtures/cedar-ember-three-banked.json");

/// The listing is a table: a count line, a blank line, a header row, then one
/// row per grant in the server's order. Every column but the last is padded to
/// its widest cell, header included, with two spaces between columns; LABEL
/// closes the row, whole when it fits 44 cells, else cut there with `...`. No
/// grant is marked next.
#[test]
fn the_listing_is_a_table_with_one_row_per_reset() {
    let lines = describe_grants("uwuclxdy", &status(THREE_BANKED));
    // EXPIRES is the local minute, as the approved listing shows it.
    let local_minute = |iso: &str| {
        chrono::DateTime::parse_from_rfc3339(iso)
            .expect("an rfc3339 stamp")
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    };
    let ends_dec = local_minute("2026-12-10T22:00:00+00:00");
    let ends_mar = local_minute("2027-03-09T07:59:00+00:00");
    let w = ends_dec
        .chars()
        .count()
        .max(ends_mar.chars().count())
        .max(7);
    let row = |id: &str, left: &str, state: &str, resets: &str, expires: &str| {
        format!("  {id:<21}  {left:<4}  {state:<5}  {resets:<12}  {expires:<w$}  ")
    };
    let head = row("ID", "LEFT", "STATE", "RESETS", "EXPIRES");
    let want = [
        (
            row("full-reset-2611", "1/1", "ready", "7d, 7d fable", &ends_dec),
            "One full limit reset (November 2026)",
        ),
        (
            row("launch-2610-fable", "1/1", "ready", "7d fable", &ends_mar),
            "New model launch: one Fable reset for Max, Team Premium and Enterprise Premium",
        ),
        (
            row(
                "launch-2610-fable-6mo",
                "1/1",
                "ready",
                "7d fable",
                &ends_mar,
            ),
            "New model launch: one more Fable reset at 6 months' tenure",
        ),
    ];
    assert_eq!(lines.len(), 6, "{lines:#?}");
    assert_eq!(lines[0], "clauth: 'uwuclxdy' has 3 limit resets available.");
    assert_eq!(lines[1], "");
    assert_eq!(lines[2], format!("{head}LABEL"));
    for (line, (cells, label)) in lines[3..].iter().zip(want) {
        let tail = line
            .strip_prefix(&cells)
            .unwrap_or_else(|| panic!("row {line:?} opens with {cells:?}"));
        // CLI text is ASCII, so the cut marks itself with `...`; widths are
        // drawn cells, as the terminal shows them.
        let cells_of = |s: &str| ratatui::text::Span::raw(s).width();
        if cells_of(label) <= 44 {
            assert_eq!(tail, label, "a label that fits 44 cells prints whole");
        } else {
            let kept = tail
                .strip_suffix("...")
                .unwrap_or_else(|| panic!("LABEL {tail:?} is cut with `...`"));
            assert!(
                !kept.is_empty() && label.starts_with(kept) && cells_of(tail) <= 44,
                "LABEL {tail:?} keeps a prefix of {label:?} within 44 cells"
            );
        }
    }
}

/// A spend names its grant: any grant the server lists can be picked by id,
/// not only the one it names next, and an id it does not list is refused.
#[test]
fn a_spend_takes_the_grant_its_id_names() {
    let status = status(THREE_BANKED);
    let picked = status
        .try_spend_grant_id("launch-2610-fable-6mo")
        .map(|g| g.id.as_str());
    assert_eq!(picked.ok(), Some("launch-2610-fable-6mo"));
    assert!(
        status.try_spend_grant_id("not-listed").is_err(),
        "an id the server does not list is refused"
    );
}
