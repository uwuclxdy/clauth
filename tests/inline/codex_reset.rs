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

/// The list body codex's own contract test pins, extra fields and all: every
/// field codex reads parses, and the ones it ignores are ignored here too.
#[test]
fn use_reset_list_parses_codexs_body_and_ignores_the_rest() {
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
        sparse.next_to_use().is_none(),
        "no status is not 'available'"
    );
}

/// The credit spent is the one that would expire first: available only, a
/// `codex_rate_limits` credit over any other type, then the earliest expiry,
/// a credit with no expiry after every dated one, and the earliest grant as
/// the tie-break.
#[test]
fn use_reset_picks_the_available_credit_that_expires_first() {
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
    assert_eq!(
        credits.next_to_use().map(|c| c.id.as_str()),
        Some("soon-granted-first"),
        "earliest expiry, the same instant spelled two ways ties, the earlier grant breaks it"
    );

    let undated_last = list_of(
        vec![
            credit(
                "no-expiry",
                "codex_rate_limits",
                "available",
                "2026-01-01T00:00:00Z",
                None,
            ),
            credit(
                "garbled",
                "codex_rate_limits",
                "available",
                "2026-01-01T00:00:00Z",
                Some("soon"),
            ),
            credit(
                "dated",
                "codex_rate_limits",
                "available",
                "2026-01-09T00:00:00Z",
                Some("2027-01-01T00:00:00Z"),
            ),
        ],
        3,
    );
    assert_eq!(
        undated_last.next_to_use().map(|c| c.id.as_str()),
        Some("dated"),
        "a credit that can wait (no expiry, or one that does not parse) goes last"
    );

    let only_other_type = list_of(
        vec![credit(
            "x",
            "something_else",
            "available",
            "2026-01-01T00:00:00Z",
            None,
        )],
        1,
    );
    assert_eq!(
        only_other_type.next_to_use().map(|c| c.id.as_str()),
        Some("x"),
        "another type is preferred against, never refused"
    );

    let none = list_of(
        vec![credit(
            "r",
            "codex_rate_limits",
            "redeemed",
            "2026-01-01T00:00:00Z",
            None,
        )],
        0,
    );
    assert!(none.next_to_use().is_none());
    assert!(list_of(Vec::new(), 0).next_to_use().is_none());
}

/// `reset` and `already_redeemed` are both done (codex reads them so);
/// `nothing_to_reset` spent nothing; a code codex does not define is kept
/// verbatim rather than failing to parse after the credit may be gone.
#[test]
fn use_reset_consume_codes_map_to_their_outcomes() {
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

/// What each outcome prints or fails with. A success is one `clauth: ` line
/// (a caller shows it as it stands); only a success says the reset was
/// used, and every uncertain failure says to look before retrying.
#[test]
fn use_reset_outcome_lines_say_what_was_spent() {
    let credits = list_of(Vec::new(), 3);
    let reply = |code: &str, windows_reset: i64| ConsumeReply {
        code: code.to_string(),
        windows_reset,
    };

    assert_eq!(
        outcome_line("work", &credits, &reply("reset", 2)),
        Ok("clauth: used a usage-limit reset on 'work': 2 windows reopened, 2 left.".to_string())
    );
    assert_eq!(
        outcome_line(
            "work",
            &list_of(Vec::new(), 1),
            &reply("already_redeemed", 1)
        ),
        Ok("clauth: used a usage-limit reset on 'work': 1 window reopened, 0 left.".to_string())
    );
    assert_eq!(
        outcome_line("work", &credits, &reply("nothing_to_reset", 0)),
        Err("there is nothing to reset on 'work' right now, so no reset was used".to_string())
    );
    let no_credit = outcome_line("work", &credits, &reply("no_credit", 0)).expect_err("fails");
    assert!(no_credit.contains("no longer available"), "{no_credit}");
    assert!(
        no_credit.contains("clauth use-reset work --list"),
        "{no_credit}"
    );
    let unknown = outcome_line("work", &credits, &reply("weird\u{1b}[2J", 0)).expect_err("fails");
    assert!(unknown.contains("unconfirmed"), "{unknown}");
    assert!(
        unknown.contains("\"weird\\u{1b}[2J\""),
        "quoted and escaped: {unknown}"
    );
    assert!(unknown.contains("before retrying"), "{unknown}");
}

/// A failed list spent nothing and says so, whatever the failure; a failed
/// consume says so only on a 401 — every other failure leaves the outcome open.
#[test]
fn use_reset_failure_lines_separate_nothing_spent_from_unconfirmed() {
    for err in [
        ResetCallError::Unauthorized,
        ResetCallError::Status(503),
        ResetCallError::Transport,
        ResetCallError::Parse,
    ] {
        let line = list_failure("work", &err);
        assert!(line.ends_with("no reset was used"), "{err:?}: {line}");
    }
    assert!(list_failure("work", &ResetCallError::Status(503)).contains("HTTP 503"));
    assert!(
        list_failure("work", &ResetCallError::Unauthorized)
            .contains("rejected the stored access token")
    );

    let rejected = consume_failure("work", &ResetCallError::Unauthorized);
    assert!(
        rejected.contains("rejected the stored access token"),
        "{rejected}"
    );
    assert!(rejected.ends_with("no reset was used"), "{rejected}");
    let transport = consume_failure("work", &ResetCallError::Transport);
    assert!(
        transport.contains("may or may not have gone through"),
        "{transport}"
    );
    assert!(
        transport.contains("clauth use-reset work --list"),
        "{transport}"
    );
    let status = consume_failure("work", &ResetCallError::Status(500));
    assert!(
        status.contains("HTTP 500") && status.contains("before retrying"),
        "{status}"
    );
    let parse = consume_failure("work", &ResetCallError::Parse);
    assert!(
        parse.contains("may or may not have gone through"),
        "{parse}"
    );
}

/// The prompt names the credit, its expiry, and "1 of N"; the listing marks
/// the credit the prompt would name. A missing title falls back to the generic
/// name, and a count lagging its own list never reads "1 of 0".
#[test]
fn use_reset_prompt_and_listing_name_the_credit_that_would_be_spent() {
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

    let prompt = use_reset_prompt("work", &credits, &dated);
    assert!(
        prompt
            .starts_with("clauth: use a usage-limit reset on 'work'? usage-limit reset · expires "),
        "{prompt}"
    );
    assert!(prompt.contains("· 1 of 2 available."), "{prompt}");
    assert!(prompt.contains("cannot be undone"), "{prompt}");
    let titled_prompt = use_reset_prompt("work", &list_of(vec![titled.clone()], 0), &titled);
    assert!(
        titled_prompt.contains("Full reset (Weekly + 5 hr) · no expiry · 1 of 1 available."),
        "control characters dropped, count floored at one: {titled_prompt}"
    );

    let lines = describe_reset_credits("work", &credits);
    assert_eq!(
        lines[0],
        "clauth: 'work' has 2 usage-limit resets available."
    );
    assert_eq!(lines.len(), 3);
    assert!(
        lines[1].starts_with("    Full reset (Weekly + 5 hr) — available, no expiry, granted "),
        "{}",
        lines[1]
    );
    assert!(
        lines[2].starts_with("  * usage-limit reset — available, expires "),
        "{}",
        lines[2]
    );
    assert!(lines[2].ends_with("(used next)"), "{}", lines[2]);

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
        lines[0],
        "clauth: no usage-limit resets available on 'work'."
    );
    assert!(!lines[1].contains("used next"), "{}", lines[1]);
}

/// An expiry or grant stamp that does not parse is shown as sent, but with its
/// control characters dropped: an escape sequence in it would otherwise reach
/// the terminal through both the `[y/N]` prompt and `--list`.
#[test]
fn use_reset_unparseable_stamps_reach_the_terminal_without_control_characters() {
    let hostile = "2026-01-01T00:00:00\u{1b}]52;c;AAAA\u{7}";
    let c = credit(
        "h",
        "codex_rate_limits",
        "available",
        hostile,
        Some(hostile),
    );
    let credits = list_of(vec![c.clone()], 1);

    let prompt = use_reset_prompt("work", &credits, &c);
    let lines = describe_reset_credits("work", &credits);
    for text in std::iter::once(&prompt).chain(lines.iter()) {
        assert!(!text.chars().any(char::is_control), "{text:?}");
    }
    assert!(
        prompt.contains("expires 2026-01-01T00:00:00]52;c;AAAA"),
        "{prompt}"
    );
    assert!(
        lines[1].contains("granted 2026-01-01T00:00:00]52;c;AAAA"),
        "{}",
        lines[1]
    );
}

/// The idempotency key is a v4 UUID: 8-4-4-4-12 lowercase hex, version nibble
/// 4, variant bits 10. Fresh per call.
#[test]
fn use_reset_redeem_request_id_is_a_fresh_v4_uuid() {
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
/// account id (only when there is one), `codex-cli`, JSON accept.
#[test]
fn use_reset_list_and_consume_send_codexs_request() {
    let list_body = r#"{"credits": [{"id": "c-1", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-01T00:00:00Z"}], "available_count": 1}"#;
    let (addr, handle) = crate::testutil::serve_endpoints_raw(3, move |path, _i| {
        if path.ends_with("/consume") {
            (200, r#"{"code": "reset", "windows_reset": 2}"#.to_string())
        } else {
            (200, list_body.to_string())
        }
    });
    let urls = ResetUrls::under(&addr);

    let listed = list_reset_credits_at(&urls.list, "at.secret", Some("acc-1")).expect("200 lists");
    assert_eq!(listed.next_to_use().map(|c| c.id.as_str()), Some("c-1"));
    let reply = consume_reset_credit_at(
        &urls.consume,
        "at.secret",
        Some("acc-1"),
        "11111111-2222-4333-8444-555555555555",
        "c-1",
    )
    .expect("200 consumes");
    assert_eq!(reply.outcome(), ConsumeOutcome::Reset { windows_reset: 2 });
    list_reset_credits_at(&urls.list, "at.secret", Some("  ")).expect("200 lists");

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
        assert_eq!(header("user-agent").as_deref(), Some("codex-cli"));
        assert_eq!(header("accept").as_deref(), Some("application/json"));
    }
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
fn use_reset_statuses_map_to_their_errors_without_a_retry() {
    let (addr, handle) = crate::testutil::serve_endpoints_raw(4, |_path, i| match i {
        0 => (401, r#"{"detail":"stale"}"#.to_string()),
        1 => (401, r#"{"detail":"stale"}"#.to_string()),
        2 => (503, "busy".to_string()),
        _ => (200, "[]".to_string()),
    });
    let urls = ResetUrls::under(&addr);
    assert_eq!(
        list_reset_credits_at(&urls.list, "at", None),
        Err(ResetCallError::Unauthorized)
    );
    assert_eq!(
        consume_reset_credit_at(&urls.consume, "at", None, "id", "c"),
        Err(ResetCallError::Unauthorized)
    );
    assert_eq!(
        consume_reset_credit_at(&urls.consume, "at", None, "id", "c"),
        Err(ResetCallError::Status(503))
    );
    assert_eq!(
        consume_reset_credit_at(&urls.consume, "at", None, "id", "c"),
        Err(ResetCallError::Parse)
    );
    assert_eq!(handle.join().expect("join stub").len(), 4);
}

/// Nothing listening is a transport failure, which the consume reports as
/// unconfirmed. A port that was just released refuses the connect at once.
#[test]
fn use_reset_transport_failure_is_reported_not_retried() {
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    let urls = ResetUrls::under(&format!("http://127.0.0.1:{port}"));
    assert_eq!(
        list_reset_credits_at(&urls.list, "at", None),
        Err(ResetCallError::Transport)
    );
    let err = consume_reset_credit_at(&urls.consume, "at", None, "id", "c").expect_err("no answer");
    assert_eq!(err, ResetCallError::Transport);
    assert!(consume_failure("work", &err).contains("may or may not have gone through"));
}
