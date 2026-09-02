#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::usage::PlanInfo;

// The centralized diagnostics are load-bearing precisely because they render on
// three surfaces from one definition; these pin that one head reaches all three
// without drifting, and that the CLI/log and toast forms differ only in the
// head↔detail separator.

#[test]
fn login_expired_shares_one_head_across_line_and_toast() {
    let m = login_expired(&crate::profile::ProfileName::from("work"));
    assert_eq!(
        m.line(),
        "login for 'work' has expired: refresh token revoked or invalid: run clauth login work"
    );
    assert_eq!(
        m.toast(),
        "login for 'work' has expired\nrefresh token revoked or invalid: run clauth login work"
    );
    // The bold toast head is exactly the line() prefix before the separator.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "login for 'work' has expired"
    );
}

#[test]
fn refresh_transient_carries_the_error_in_the_detail() {
    let m = refresh_transient(
        &crate::profile::ProfileName::from("flaky"),
        &Transient::new(
            Cause::Endpoint("could not reach anthropic"),
            Retry::Connection,
        ),
    );
    assert_eq!(
        m.line(),
        "could not refresh 'flaky' before switching: could not reach anthropic: check your \
         connection and retry"
    );
    // The head stays fixed-length regardless of the (arbitrary, possibly long)
    // error text, so it can never wrap the toast's bold first line.
    assert_eq!(
        m.toast().lines().next().unwrap(),
        "could not refresh 'flaky' before switching"
    );
    assert_eq!(
        m.toast().lines().nth(1).unwrap(),
        "could not reach anthropic: check your connection and retry"
    );
}

/// The whole reason the kind travels inside the value: `check your connection`
/// is wrong advice for a throttle or a 5xx, and it used to be appended
/// unconditionally to all four `AuthGate::Transient` causes — including the
/// rotation-lock one, which already tells you to retry for a different reason.
#[test]
fn the_retry_hint_follows_the_kind_not_the_call_site() {
    let connection = Transient::new(
        Cause::Endpoint("could not reach anthropic"),
        Retry::Connection,
    );
    assert_eq!(
        connection.text(),
        "could not reach anthropic: check your connection and retry"
    );

    let wait = Transient::with_status(
        Cause::Endpoint("anthropic is throttling requests"),
        429,
        Retry::Wait,
    );
    assert_eq!(
        wait.text(),
        "anthropic is throttling requests: retry in a moment"
    );
    assert!(
        !wait.text().contains("connection"),
        "a 429 must never be blamed on the operator's connection: {}",
        wait.text()
    );

    // `Stated` adds nothing: the cause already names its own next step, and a
    // second one contradicts it.
    let stated = Transient::new(
        Cause::RotationLockUnavailable("work".to_string()),
        Retry::Stated,
    );
    assert_eq!(
        stated.text(),
        "could not lock 'work' for a token refresh; check permissions on ~/.clauth"
    );
}

/// The pairing rule is structural, not a convention: a `Cause` whose copy
/// names the operator's next step refuses every retry that appends advice —
/// `Wait`, `Connection`, `Restart` — at construction. The appended hint would
/// duplicate the cause's advice (`RotationLockHeld`) or contradict it (a
/// permissions check followed by `check your connection and retry`). An
/// `ends_with` guard cannot reach this — the cause text ends in a comma and
/// the suffix begins with a colon — so the refusal has to sit in the
/// constructor itself. `Stated` appends nothing and pairs with every arm.
#[test]
fn self_prescribing_arm_refuses_every_suffixed_retry_at_construction() {
    for retry in [Retry::Wait, Retry::Connection, Retry::Restart] {
        let retry_name = format!("{retry:?}");
        let caught = std::panic::catch_unwind(|| {
            Transient::new(Cause::RotationLockHeld("work".to_string()), retry);
        });
        let msg = caught.expect_err(
            "a self-prescribing arm paired with a suffix-bearing retry must refuse at \
             construction",
        );
        let msg = msg
            .downcast_ref::<String>()
            .expect("refusal message as String");
        assert!(
            msg.contains("RotationLockHeld") && msg.contains(&retry_name),
            "the refusal must name the arm and the retry, got: {msg}"
        );
    }

    // `with_status` is the same construction with a status in the middle, and
    // the same pairing rule: the stutter still ships on the status-bearing
    // form.
    let caught = std::panic::catch_unwind(|| {
        Transient::with_status(
            Cause::SidecarMisfilled("work".to_string()),
            400,
            Retry::Wait,
        );
    });
    assert!(
        caught.is_err(),
        "the status-bearing constructor must refuse the pairing too"
    );
}

/// The CLI/daemon surfaces name the HTTP status; the toast and MCP forms do not.
/// Asserted together so neither half can drift alone — a status that silently
/// stops reaching stderr looks exactly like one that was never added.
#[test]
fn only_the_status_bearing_form_names_the_status() {
    let t = Transient::with_status(
        Cause::Endpoint("anthropic is having trouble"),
        503,
        Retry::Wait,
    );
    assert_eq!(
        t.text_with_status(),
        "anthropic is having trouble (HTTP 503): retry in a moment"
    );
    assert_eq!(t.text(), "anthropic is having trouble: retry in a moment");
    assert!(
        !t.text().contains("503"),
        "the canned form must not leak the status: {}",
        t.text()
    );

    assert_eq!(
        refresh_transient_cli(&crate::profile::ProfileName::from("work"), &t).line(),
        "could not refresh 'work' before switching: anthropic is having trouble (HTTP 503): \
         retry in a moment"
    );
    assert!(
        !refresh_transient(&crate::profile::ProfileName::from("work"), &t)
            .line()
            .contains("503"),
        "the non-CLI constructor must stay status-free"
    );

    // A failure that never saw a status has nothing honest to add, so the two
    // forms coincide rather than inventing one.
    let no_status = Transient::new(
        Cause::Endpoint("could not reach anthropic"),
        Retry::Connection,
    );
    assert_eq!(no_status.text_with_status(), no_status.text());
}

/// Every arm of the closed cause set renders, and the name-bearing ones
/// interpolate the profile. Pinned because `Cause` is what replaced a
/// `cause: String` that would have accepted a response body: if an arm is ever
/// added, this is where its copy has to be stated rather than passed in — ALL
/// of it, or a blanked-out arm ships mute.
///
/// The table also pins the pairing rule per arm, against every `Retry`:
/// `Stated` renders the bare copy, and each suffix-bearing retry (`Wait`,
/// `Connection`, `Restart`) is refused when the arm's copy names its own next
/// step (`names_next_step` — the row states it independently of the
/// constructor's own classification, so a dropped arm reds here instead of
/// silently accepting the stutter) and appended when it does not.
#[test]
fn every_transient_cause_renders_its_own_copy() {
    struct Row {
        cause: Cause,
        bare: &'static str,
        names_next_step: bool,
    }
    for Row {
        cause,
        bare,
        names_next_step,
    } in [
        Row {
            cause: Cause::Endpoint("anthropic is throttling requests"),
            bare: "anthropic is throttling requests",
            names_next_step: false,
        },
        Row {
            cause: Cause::RotationLockUnavailable("work".to_string()),
            bare: "could not lock 'work' for a token refresh; check permissions on ~/.clauth",
            names_next_step: true,
        },
        Row {
            cause: Cause::InternalLock,
            bare: "clauth hit an internal lock error, restart clauth",
            names_next_step: true,
        },
        Row {
            cause: Cause::PersistFailed("work".to_string()),
            bare: "refreshed 'work' but failed to persist the rotated tokens",
            names_next_step: false,
        },
        Row {
            cause: Cause::SidecarWriteFailed("work".to_string()),
            bare: "could not write 'work' session token · check permissions on ~/.clauth",
            names_next_step: true,
        },
        Row {
            cause: Cause::LiveSessionOnRotatingChain("work".to_string()),
            bare: "'work' has a live clauth start session holding its rotating chain (it \
                    started before the rolling token was armed); restart that session or \
                    retry once it ends",
            names_next_step: true,
        },
        Row {
            cause: Cause::RotationLockHeld("work".to_string()),
            bare: "'work' has a token rotation in progress, retry in a moment",
            names_next_step: true,
        },
        Row {
            cause: Cause::RollingGrantUnrecorded("work".to_string()),
            bare: "'work' usage chain has no recorded grant beyond the setup-token scopes, \
                    so a rolling bearer cannot be told from a mint · run `clauth login work` \
                    to record the chain's real grant",
            names_next_step: true,
        },
        Row {
            cause: Cause::SidecarMisfilled("work".to_string()),
            bare: "'work' session token holds a rotating pair and no live mint backup exists \
                    to heal it · re-capture with `clauth login work --setup-token`",
            names_next_step: true,
        },
        Row {
            cause: Cause::StateLockBusy("work".to_string()),
            bare: "another clauth process holds ~/.clauth's state lock · 'work' left unchanged",
            names_next_step: false,
        },
        Row {
            cause: Cause::StateLockUnavailable("work".to_string()),
            bare: "could not lock 'work' for a token refresh; check permissions on ~/.clauth",
            names_next_step: true,
        },
    ] {
        assert_eq!(Transient::new(cause.clone(), Retry::Stated).text(), bare);
        for (retry, suffix) in [
            (Retry::Wait, ": retry in a moment"),
            (Retry::Connection, ": check your connection and retry"),
            (Retry::Restart, ": run clauth login again for a fresh code"),
        ] {
            let retry_name = format!("{retry:?}");
            if names_next_step {
                let caught = std::panic::catch_unwind(|| {
                    Transient::new(cause.clone(), retry);
                });
                assert!(
                    caught.is_err(),
                    "a self-prescribing arm must refuse {retry_name}: {bare}"
                );
            } else {
                assert_eq!(
                    Transient::new(cause.clone(), retry).text(),
                    format!("{bare}{suffix}"),
                    "an arm that names no next step takes {retry_name}'s advice: {bare}"
                );
            }
        }
    }
}

/// The re-login-only causes — and ONLY those — read as permanent to the
/// scheduler's pacing: a minutes-scale retry against a condition no in-process
/// retry can clear is log noise, while parking a genuinely transient cause on
/// the six-hour leash would silence recovery the next scan could have made.
#[test]
fn only_the_relogin_causes_read_as_permanent() {
    for (cause, want) in [
        (Cause::RollingGrantUnrecorded("work".to_string()), true),
        (Cause::SidecarMisfilled("work".to_string()), true),
        (Cause::Endpoint("anthropic is throttling requests"), false),
        (Cause::RotationLockHeld("work".to_string()), false),
        (Cause::RotationLockUnavailable("work".to_string()), false),
        (Cause::SidecarWriteFailed("work".to_string()), false),
        (Cause::LiveSessionOnRotatingChain("work".to_string()), false),
        (Cause::InternalLock, false),
        (Cause::PersistFailed("work".to_string()), false),
        (Cause::StateLockBusy("work".to_string()), false),
        (Cause::StateLockUnavailable("work".to_string()), false),
    ] {
        let t = Transient::new(cause, Retry::Stated);
        assert_eq!(t.permanent_until_relogin(), want, "{}", t.text());
    }
}

/// `detail()` is what lets a surface whose own first line states the condition
/// avoid restating it. The fallback arm matters: a detail-less `Message` must
/// still render something, or a caller would mint copy of its own.
#[test]
fn detail_returns_the_next_step_alone_and_falls_back_to_the_head() {
    assert_eq!(
        login_expired(&crate::profile::ProfileName::from("work")).detail(),
        "refresh token revoked or invalid: run clauth login work"
    );
    let bare = Message {
        head: "done".to_string(),
        detail: None,
    };
    assert_eq!(bare.detail(), "done");
}

#[test]
fn line_and_toast_collapse_to_the_head_when_detail_is_absent() {
    let m = Message {
        head: "done".to_string(),
        detail: None,
    };
    assert_eq!(m.line(), "done");
    assert_eq!(m.toast(), "done");
}

#[test]
fn resolve_in_tui_names_the_clauth_surface() {
    assert!(RESOLVE_IN_TUI.contains("clauth TUI"));
}

#[test]
fn format_pct_drops_trailing_zero_on_whole_numbers() {
    assert_eq!(format_pct(42.0), "42%");
    assert_eq!(format_pct(0.0), "0%");
    assert_eq!(format_pct(100.0), "100%");
}

#[test]
fn format_pct_shows_fractional_percent() {
    assert_eq!(format_pct(42.3), "42.3%");
}

/// `local_stamp` is the one prose-stamp formatter: epoch seconds → `YYYY-MM-DD
/// HH:MM:SS` in local wall clock. Pinned on a fixed epoch so the SHAPE asserts
/// independently of the operator's zone — the wall-clock digits shift with the
/// zone, the separators do not.
#[test]
fn local_stamp_renders_the_local_wall_clock_shape() {
    let stamp = local_stamp(0).unwrap();
    assert_eq!(stamp.len(), 19, "{stamp}");
    for (i, c) in stamp.chars().enumerate() {
        match i {
            4 | 7 => assert_eq!(c, '-', "{stamp}"),
            10 => assert_eq!(c, ' ', "{stamp}"),
            13 | 16 => assert_eq!(c, ':', "{stamp}"),
            _ => assert!(c.is_ascii_digit(), "non-digit at {i} in {stamp}"),
        }
    }
}

#[test]
fn account_tier_reads_the_fetched_tier_only_the_canceled_marker_is_on_the_status_line() {
    let mut canceled = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    canceled.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: Some("canceled".to_string()),
        }),
        ..Default::default()
    });
    assert_eq!(account_tier(&canceled), Some(PlanTier::Free));

    // A genuine, never-subscribed free account looks the same here — the
    // canceled distinction lives on the status line, not the plan tier.
    let mut free = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    free.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(account_tier(&free), Some(PlanTier::Free));
}

/// An unfetched plan has no tier at all. `account_tier` says so with `None`
/// so each surface picks its own no-data form — a bare "Claude" here read as a
/// real plan, and shipped to the Overview chip, the Usage `plan` row and
/// `which --json`'s `tier` alike.
#[test]
fn account_tier_reports_no_tier_for_an_unfetched_plan() {
    // No credentials at all: nothing on disk claims a tier.
    let bare = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    assert_eq!(account_tier(&bare), None);

    // A token whose `subscription_type` is not one clauth classifies is the
    // same "we do not know" — never a fabricated tier.
    let mut unclassified = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    unclassified.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("something_new".into()),
        }),
    });
    assert_eq!(account_tier(&unclassified), None);

    // A fetched plan whose tier never classified, with no token claim to fall
    // through to, reads the same way.
    let mut unknown_plan = crate::testutil::blank_profile(&crate::profile::ProfileName::from("c"));
    unknown_plan.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Unknown,
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(account_tier(&unknown_plan), None);
}

/// An UNCLASSIFIED fetched plan is not an answer, so it falls through to the
/// token claim exactly the way `profile_json::tier_label` does. Short-circuiting
/// on it instead left this surface reporting "no data" while `status.json` showed
/// a tier for the same account at the same instant.
#[test]
fn account_tier_falls_through_an_unclassified_fetched_plan_to_the_token() {
    let token = |sub: &str| {
        Some(crate::profile::ClaudeCredentials {
            claude_ai_oauth: Some(crate::profile::OAuthToken {
                access_token: "at".into(),
                refresh_token: None,
                expires_at: None,
                scopes: None,
                subscription_type: Some(sub.into()),
            }),
        })
    };
    let plan = |tier| {
        Some(crate::usage::UsageInfo {
            plan: Some(PlanInfo {
                tier,
                subscription_status: None,
            }),
            ..Default::default()
        })
    };

    let mut unclassified = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    unclassified.usage = plan(PlanTier::Unknown);
    unclassified.credentials = token("max");
    assert_eq!(
        account_tier(&unclassified),
        Some(PlanTier::Max(None)),
        "an unclassified fetched tier must not mask the token's claim"
    );

    // The other arm of the same branch: a fetched tier that DID classify still
    // wins over a disagreeing token, so the fall-through cannot invert priority.
    let mut disagreeing = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    disagreeing.usage = plan(PlanTier::Max(Some(20)));
    disagreeing.credentials = token("pro");
    assert_eq!(
        account_tier(&disagreeing),
        Some(PlanTier::Max(Some(20))),
        "the fetched tier is the better source and still wins"
    );
}

/// A `Free` login round-trips end to end: `login_profile_from_raw` stores
/// `"free"`, and this surface reads it back as the plan rather than the no-data
/// form. Free has no `has_claude_*` flag to recover it, so the token is the only
/// pre-fetch source it has.
#[test]
fn account_tier_reads_back_a_free_logins_stored_token() {
    let mut free = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    free.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("free".into()),
        }),
    });
    assert_eq!(account_tier(&free), Some(PlanTier::Free));
}

/// The other direction: a real tier still renders, and `Free` is untouched by
/// the unfetched-plan change.
#[test]
fn account_tier_still_renders_every_known_tier() {
    let mut fetched = crate::testutil::blank_profile(&crate::profile::ProfileName::from("a"));
    fetched.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Max(Some(20)),
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(account_tier(&fetched), Some(PlanTier::Max(Some(20))));

    let mut free = crate::testutil::blank_profile(&crate::profile::ProfileName::from("b"));
    free.usage = Some(crate::usage::UsageInfo {
        plan: Some(PlanInfo {
            tier: PlanTier::Free,
            subscription_status: None,
        }),
        ..Default::default()
    });
    assert_eq!(account_tier(&free), Some(PlanTier::Free));

    let mut token_only = crate::testutil::blank_profile(&crate::profile::ProfileName::from("c"));
    token_only.credentials = Some(crate::profile::ClaudeCredentials {
        claude_ai_oauth: Some(crate::profile::OAuthToken {
            access_token: "at".into(),
            refresh_token: None,
            expires_at: None,
            scopes: None,
            subscription_type: Some("pro".into()),
        }),
    });
    assert_eq!(account_tier(&token_only), Some(PlanTier::Pro));
}

/// The splitter family's bytes, pinned here because every routing test asserts
/// `third_party_dead_chain_copy`'s output against a call of the same
/// constructor: those pin WHICH sentence is selected and nothing about what it
/// renders, so a reword ships green through all of them. The two older
/// sentences carry end-to-end literal pins at their routing sites; this is the
/// third one's only guard, and all three are owner-ruled verbatim.
#[test]
fn the_split_state_sentences_render_their_ruled_bytes() {
    let name = crate::profile::ProfileName::from("qwen");
    assert_eq!(
        third_party_keyless(&name),
        "profile has no api key: qwen (run `clauth login qwen --api-key <key>`)"
    );
    assert_eq!(
        third_party_dead_chain(&name),
        "stored OAuth chain is dead, its api key still works: qwen \
         (run `clauth login qwen --api-key <key>` to clear the quarantine)"
    );
    assert_eq!(
        third_party_dead_console(&name),
        "console session expired, stored OAuth chain is dead: qwen \
         (run `clauth login qwen` to re-capture the console; the api key still serves inference)"
    );
}
