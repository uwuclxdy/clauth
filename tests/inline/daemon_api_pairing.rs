//! Pairing codes: their shape, the locked redemption behind `POST
//! /api/v1/pair`, and the `pair` wait. All disk state sits in a
//! [`HomeSandbox`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

use std::net::SocketAddr;

use crate::daemon::api::http::{Request, Response};
use crate::daemon::api::routes::{ApiContext, handle};
use crate::testutil::HomeSandbox;

fn name(raw: &str) -> DeviceName {
    DeviceName::parse(raw).expect("a valid device name")
}

fn peer() -> SocketAddr {
    SocketAddr::from(([192, 0, 2, 7], 50_000))
}

fn ctx() -> std::sync::Arc<ApiContext> {
    let config = std::sync::Arc::new(crate::lockorder::RankedMutex::new(
        crate::profile::AppConfig {
            state: crate::profile::AppState::default(),
            profiles: Vec::new(),
        },
    ));
    ApiContext::for_tests(
        config,
        clauth_dir().expect("dir").join("status.json"),
        None,
        crate::daemon::api::panes::absent_probe(),
    )
}

fn post_pair(ctx: &ApiContext, body: &str) -> Response {
    handle(
        ctx,
        &Request {
            method: "POST".to_string(),
            path: "/api/v1/pair".to_string(),
            query: String::new(),
            bearer: None,
            authorization_seen: false,
            if_none_match: None,
            body: body.as_bytes().to_vec(),
            keep_alive: true,
            ws: Default::default(),
            cross_origin: Default::default(),
            content_type: crate::daemon::api::http::SoloHeader::One("application/json".to_string()),
        },
        peer(),
    )
    .response
}

/// The body a device posts, carrying the code the way a person reads it off
/// the screen.
fn pair_body(code: &Code) -> String {
    serde_json::json!({ "code": code.to_string() }).to_string()
}

fn body_json(resp: &Response) -> serde_json::Value {
    serde_json::from_slice(&resp.body).expect("a json body")
}

fn live_pairing() -> Option<PairingFile> {
    read_pairing(&pairing_path().expect("path")).expect("read")
}

/// A well-formed code that is not `code`.
fn wrong_code(code: &Code) -> Code {
    Code(
        if code.0 == "00000000" {
            "11111111"
        } else {
            "00000000"
        }
        .to_string(),
    )
}

fn paired(redeemed: &Redeemed) -> bool {
    matches!(redeemed, Redeemed::Paired { .. })
}

// ── the code ────────────────────────────────────────────────────────────────

#[test]
fn the_alphabet_is_crockford_base32() {
    assert_eq!(ALPHABET, b"0123456789ABCDEFGHJKMNPQRSTVWXYZ");
    let unique: std::collections::BTreeSet<&u8> = ALPHABET.iter().collect();
    assert_eq!(unique.len(), 32);
    for excluded in *b"ILOU" {
        assert!(!ALPHABET.contains(&excluded), "{}", char::from(excluded));
    }
}

/// All 40 bits reach the code, five to a character: the low bits pick the last
/// character, the high bits the first, and nothing past bit 40 leaks in.
#[test]
fn a_code_spells_forty_bits_five_to_a_character() {
    for (bits, spelled) in [
        (0, "00000000"),
        (1, "00000001"),
        (31, "0000000Z"),
        (32, "00000010"),
        (31 << 35, "Z0000000"),
        ((1 << 40) - 1, "ZZZZZZZZ"),
        (1 << 40, "00000000"),
    ] {
        assert_eq!(Code::from_bits(bits).0, spelled, "{bits:#x}");
    }
}

/// Every position varies across codes the CSPRNG drew. A generator that fed
/// fewer than 40 bits would pin its top characters, and 256 draws leave a
/// uniform one no realistic chance of repeating a character at any position.
#[test]
fn generated_codes_use_the_alphabet_and_vary_at_every_position() {
    let codes: Vec<Code> = (0..256)
        .map(|_| Code::generate().expect("generate"))
        .collect();
    for code in &codes {
        assert_eq!(code.0.len(), 8);
        assert!(code.0.bytes().all(|b| ALPHABET.contains(&b)), "{}", code.0);
    }
    for position in 0..8 {
        let seen: std::collections::BTreeSet<u8> = codes
            .iter()
            .map(|code| code.0.as_bytes()[position])
            .collect();
        assert!(
            seen.len() > 1,
            "position {position} never varied across 256 codes"
        );
    }
}

#[test]
fn a_code_shows_as_two_fours_and_reads_back() {
    let code = Code("ABCD2345".to_string());
    assert_eq!(code.to_string(), "ABCD-2345");
    assert!(Code::normalize(&code.to_string()) == Some(code));
}

#[test]
fn normalization_folds_case_separators_and_look_alikes() {
    for (typed, canonical) in [
        ("abcd-efgh", "ABCDEFGH"),
        (" ab cd\tef-gh\n", "ABCDEFGH"),
        ("--ABCD--EFGH--", "ABCDEFGH"),
        ("0OoI1iLl", "00011111"),
        ("zzzz-zzzz", "ZZZZZZZZ"),
    ] {
        let normalized = Code::normalize(typed).map(|code| code.0);
        assert_eq!(normalized.as_deref(), Some(canonical), "{typed:?}");
    }
    // Every alphabet character reads as itself in either case: a look-alike
    // fold on any of them would leave every code holding it unredeemable.
    for chunk in ALPHABET.chunks(CODE_LEN) {
        let canonical = std::str::from_utf8(chunk).expect("the alphabet is ascii");
        for typed in [canonical.to_string(), canonical.to_ascii_lowercase()] {
            let normalized = Code::normalize(&typed).map(|code| code.0);
            assert_eq!(normalized.as_deref(), Some(canonical), "{typed:?}");
        }
    }
}

#[test]
fn normalization_refuses_what_cannot_be_a_code() {
    for typed in [
        "",
        "----",
        "ABCDEFG",
        "ABCDEFGHJ",
        "ABCDEFGU",
        "ABCDEFG!",
        "ABCDEFGÅ",
        "ABCDEFG\u{ff21}",
    ] {
        assert!(
            Code::normalize(typed).is_none(),
            "{typed:?} must not be a code"
        );
    }
}

#[test]
fn a_code_and_a_redemption_never_format_their_secret() {
    let code = Code("ABCD2345".to_string());
    assert_eq!(format!("{code:?}"), "Code(<redacted>)");
    let redeemed = Redeemed::Paired {
        name: "phone".to_string(),
        tier: Tier::View,
        token: "secret-token-bytes".to_string(),
    };
    assert!(!format!("{redeemed:?}").contains("secret-token-bytes"));
}

// ── the pairing file ────────────────────────────────────────────────────────

/// Unix-only for the modes: Windows has none, and `atomic_write_600` writes
/// plainly there.
#[cfg(unix)]
#[test]
fn the_pairing_file_is_owner_only_and_holds_no_code() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let body = std::fs::read_to_string(pairing_path().expect("path")).expect("read");
    assert!(
        !body.contains(&pending.code().0) && !body.contains(&pending.code().to_string()),
        "the code itself never reaches the disk"
    );
    let live = live_pairing().expect("live");
    assert_eq!(
        (live.name.as_str(), &live.tier, live.attempts_left),
        ("phone", &Tier::View, CODE_ATTEMPTS)
    );
    let loose = crate::testutil::owner_only_violations(&clauth_dir().expect("dir"));
    assert!(loose.is_empty(), "loose: {loose:#?}");
}

#[test]
fn pairing_refuses_a_name_a_device_holds() {
    let _home = HomeSandbox::new();
    devices::add(&name("phone"), Tier::View, false).expect("add");
    let Err(err) = begin(&name("PHONE"), Tier::View, false) else {
        panic!("the name is taken");
    };
    assert_eq!(
        err.to_string(),
        "a device named 'phone' already exists; revoke it first: clauth devices revoke phone"
    );
    assert!(live_pairing().is_none(), "a refused pair mints no code");
}

/// A live code holds its name: an `add` under it would make the redemption
/// fail and its waiter read the added device as its own success.
#[test]
fn a_waiting_code_holds_its_name_against_add() {
    let _home = HomeSandbox::new();
    let _pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let err =
        devices::add(&name("Phone"), Tier::Control, false).expect_err("the code holds the name");
    assert_eq!(
        err.to_string(),
        "a pairing code for 'phone' is waiting to be entered; let it finish or pick another name"
    );
}

#[test]
fn an_expired_code_holds_no_name() {
    let _home = HomeSandbox::new();
    begin_at(
        &name("phone"),
        Tier::View,
        false,
        now_epoch_secs() - CODE_TTL_SECS,
    )
    .expect("begin");
    devices::add(&name("phone"), Tier::View, false).expect("an expired code blocks nothing");
}

// ── redemption ──────────────────────────────────────────────────────────────

#[test]
fn the_right_code_mints_a_device_with_the_pairings_tier() {
    for tier in [Tier::View, Tier::Control] {
        let _home = HomeSandbox::new();
        let pending = begin(&name("phone"), tier.clone(), false).expect("begin");
        let Redeemed::Paired {
            name: minted_name,
            tier: minted_tier,
            token,
        } = redeem(pending.code()).expect("redeem")
        else {
            panic!("the right code must pair");
        };
        assert_eq!((minted_name.as_str(), &minted_tier), ("phone", &tier));
        let device = devices::authenticate(Some(&token))
            .expect("read")
            .expect("the minted token verifies");
        assert_eq!(
            (device.name.as_str(), &device.tier, &device.joined),
            ("phone", &tier, &devices::Joined::Pair)
        );
        assert!(live_pairing().is_none(), "a redeemed code is gone");
    }
}

#[test]
fn a_wrong_code_is_refused_and_costs_one_attempt() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    assert!(!paired(
        &redeem(&wrong_code(pending.code())).expect("redeem")
    ));
    assert_eq!(
        live_pairing().expect("still live").attempts_left,
        CODE_ATTEMPTS - 1
    );
    assert!(
        paired(&redeem(pending.code()).expect("redeem")),
        "a typo does not spend the code"
    );
}

/// PA-1: the fifth wrong try deletes the code, so an online guesser gets five
/// tries at 40 bits, and the burn is said once in the log.
#[test]
fn the_fifth_wrong_try_burns_the_code() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let wrong = wrong_code(pending.code());
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    for left in (1..CODE_ATTEMPTS).rev() {
        assert!(!paired(&redeem(&wrong).expect("redeem")));
        assert_eq!(live_pairing().expect("live").attempts_left, left);
    }
    assert!(!paired(&redeem(&wrong).expect("redeem")));
    assert!(
        live_pairing().is_none(),
        "the fifth wrong try deletes the code"
    );
    assert!(
        !paired(&redeem(pending.code()).expect("redeem")),
        "a burned code redeems nothing, the right one included"
    );
    assert!(
        devices::read_store()
            .expect("read")
            .named("phone")
            .is_none()
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth api: the pairing code for 'phone' was dropped after 5 wrong tries".to_string()
        ]
    );
}

/// PA-4: the code is live for the ruling's 5 minutes and not a second past
/// them, and the attempt that finds it dead deletes it. The 300 is spelled out
/// because every other test derives its expiry from `CODE_TTL_SECS`.
#[test]
fn a_code_redeems_until_its_expiry_and_not_at_it() {
    let _home = HomeSandbox::new();
    let t0 = 1_800_000_000;
    let pending = begin_at(&name("phone"), Tier::View, false, t0).expect("begin");
    assert_eq!(
        (pending.expires_at, live_pairing().expect("live").expires_at),
        (t0 + 300, epoch_secs_to_iso(t0 + 300)),
        "a code expires 300 seconds after its mint"
    );
    assert!(
        !paired(&redeem_at(pending.code(), t0 + 300).expect("redeem")),
        "at the expiry instant the code is dead"
    );
    assert!(
        live_pairing().is_none(),
        "the attempt that found it expired deleted it"
    );

    let pending = begin_at(&name("phone"), Tier::View, false, t0).expect("begin");
    assert!(
        paired(&redeem_at(pending.code(), t0 + 299).expect("redeem")),
        "one second before, it pairs"
    );
}

/// PA-2: a code pairs once.
#[test]
fn a_redeemed_code_is_refused_the_second_time() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    assert!(paired(&redeem(pending.code()).expect("first")));
    assert!(!paired(&redeem(pending.code()).expect("second")));
    assert_eq!(devices::read_store().expect("read").devices.len(), 1);
}

/// With no code waiting, a redemption answers without the state flock, so an
/// unpaired peer cannot take the cross-process lock at will.
#[test]
fn with_no_code_waiting_a_redemption_takes_no_lock() {
    let _home = HomeSandbox::new();
    let before = crate::lock::OUTERMOST_ACQUISITIONS.with(std::cell::Cell::get);
    assert!(!paired(
        &redeem(&Code("ABCD2345".to_string())).expect("redeem")
    ));
    assert_eq!(
        crate::lock::OUTERMOST_ACQUISITIONS.with(std::cell::Cell::get),
        before
    );
}

#[test]
fn with_no_code_live_a_redemption_is_refused() {
    let _home = HomeSandbox::new();
    assert!(!paired(
        &redeem(&Code("ABCD2345".to_string())).expect("redeem")
    ));
}

/// The ordering: the device list reaches the disk before the code goes, so a
/// write that fails leaves the code to redeem again.
#[test]
fn a_failed_store_write_leaves_the_code_live() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    devices::fail_next_write();
    let err = redeem(pending.code()).expect_err("the injected write failure propagates");
    assert!(
        format!("{err:#}").contains("injected failure writing the device list"),
        "{err:#}"
    );
    let live = live_pairing().expect("the code must survive a write that did not land");
    assert_eq!(
        live.attempts_left, CODE_ATTEMPTS,
        "the right code cost nothing"
    );
    assert!(paired(&redeem(pending.code()).expect("retry")));
}

// ── redemption over the route ───────────────────────────────────────────────

/// PA-3: however many requests race one right code, exactly one mints.
#[test]
fn concurrent_redemptions_of_one_code_yield_one_201() {
    const RACERS: usize = 16;
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let body = pair_body(pending.code());
    let barrier = std::sync::Barrier::new(RACERS);

    let statuses: Vec<u16> = std::thread::scope(|scope| {
        let racers: Vec<_> = (0..RACERS)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    post_pair(&ctx, &body).status
                })
            })
            .collect();
        racers
            .into_iter()
            .map(|racer| racer.join().expect("racer"))
            .collect()
    });

    assert_eq!(
        statuses.iter().filter(|status| **status == 201).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|status| *status == 201 || *status == 403),
        "{statuses:?}"
    );
    assert_eq!(devices::read_store().expect("read").devices.len(), 1);
}

/// `racers` requests, released together, each posting the same wrong code
/// against one live code: each racer's status, and every line any of them
/// logged.
fn race_wrong_codes(racers: usize) -> (Vec<u16>, Vec<String>) {
    let ctx = ctx();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let body = pair_body(&wrong_code(pending.code()));
    let barrier = std::sync::Barrier::new(racers);
    let lines = crate::logline::LogLines::new();
    let statuses: Vec<u16> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..racers)
            .map(|_| {
                scope.spawn(|| {
                    let _capture = lines.capture_here();
                    barrier.wait();
                    post_pair(&ctx, &body).status
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|racer| racer.join().expect("racer"))
            .collect()
    });
    (statuses, lines.snapshot())
}

/// PA-1 under concurrency: wrong codes racing one live code each cost one
/// attempt, because the count is read and written inside the redemption's
/// hold; a count read before it would let every racer spend the same try.
#[test]
fn concurrent_wrong_codes_each_cost_one_attempt() {
    let _home = HomeSandbox::new();
    let (statuses, logged) = race_wrong_codes(4);
    assert_eq!(statuses, vec![403; 4]);
    assert_eq!(
        live_pairing().expect("still live").attempts_left,
        CODE_ATTEMPTS - 4
    );
    assert!(logged.is_empty(), "{logged:?}");
}

/// Five wrong codes burn the code however they interleave, and the burn is
/// said once; the racers past the fifth find nothing left to guess at.
#[test]
fn concurrent_wrong_codes_burn_the_code_at_the_fifth() {
    let _home = HomeSandbox::new();
    let (statuses, logged) = race_wrong_codes(8);
    assert_eq!(statuses, vec![403; 8]);
    assert!(live_pairing().is_none(), "five wrong codes delete it");
    assert_eq!(
        logged,
        vec!["clauth api: the pairing code for 'phone' was dropped after 5 wrong tries"]
    );
}

/// The 201 carries the token, and no log line carries it or the code: the
/// pairing line names the peer and the device, nothing else.
#[test]
fn the_201_carries_the_token_and_no_log_line_carries_a_secret() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let pending = begin(&name("phone"), Tier::Control, false).expect("begin");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    let resp = post_pair(&ctx, &pair_body(pending.code()));

    assert_eq!(resp.status, 201);
    let body = body_json(&resp);
    let token = body["token"]
        .as_str()
        .expect("the token rides the 201")
        .to_string();
    assert_eq!(
        body,
        serde_json::json!({"ok": true, "name": "phone", "tier": "control", "token": token})
    );
    assert!(devices::authenticate(Some(&token)).expect("read").is_some());
    let logged = lines.snapshot();
    assert_eq!(
        logged,
        vec![format!(
            "clauth api: {} paired device 'phone' (control)",
            peer()
        )]
    );
    for line in &logged {
        for secret in [
            token.as_str(),
            pending.code().0.as_str(),
            &pending.code().to_string(),
        ] {
            assert!(!line.contains(secret), "a secret reached the log");
        }
    }
}

/// The tier in the pairing line comes off `pairing.json`, which a newer build
/// or a hand edit may have written, so it reaches the log sanitized, like the
/// device name beside it.
#[test]
fn the_pairing_line_sanitizes_the_tier_it_read() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let path = pairing_path().expect("path");
    let mut file = read_pairing(&path).expect("read").expect("live");
    file.tier = Tier::Unknown("view\nclauth api: forged".to_string());
    write_pairing(&path, &file).expect("rewrite the tier");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    assert_eq!(post_pair(&ctx, &pair_body(pending.code())).status, 201);

    assert_eq!(
        lines.snapshot(),
        vec![format!(
            "clauth api: {} paired device 'phone' (view clauth api: forged)",
            peer()
        )]
    );
}

/// One refusal shape for every failed redemption, byte for byte, so the answer
/// tells a prober nothing about the host's state.
#[test]
fn every_failed_redemption_answers_the_same_bytes() {
    let _home = HomeSandbox::new();
    let ctx = ctx();

    let none = post_pair(&ctx, r#"{"code":"ABCD-2345"}"#);

    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let wrong_body = pair_body(&wrong_code(pending.code()));
    let wrong = post_pair(&ctx, &wrong_body);
    for _ in 1..CODE_ATTEMPTS {
        post_pair(&ctx, &wrong_body);
    }
    let burned = post_pair(&ctx, &pair_body(pending.code()));

    let stale = begin_at(
        &name("phone"),
        Tier::View,
        false,
        now_epoch_secs() - CODE_TTL_SECS,
    )
    .expect("begin");
    let expired = post_pair(&ctx, &pair_body(stale.code()));

    assert_eq!(
        (none.status, body_json(&none)),
        (
            403,
            serde_json::json!({
                "ok": false,
                "error": "pairing_refused",
                "reason": "that code did not pair a device: check it, or run `clauth devices \
                           pair <name>` on the host for a new one",
            })
        )
    );
    for (case, resp) in [
        ("wrong", &wrong),
        ("burned", &burned),
        ("expired", &expired),
    ] {
        assert!(
            resp.status == none.status && resp.body == none.body,
            "{case} answered differently from no code at all"
        );
    }
}

/// A body holding no code is a 400 judged before the pairing is read, so it
/// costs no attempt.
#[test]
fn a_body_holding_no_code_is_400_and_costs_no_attempt() {
    let _home = HomeSandbox::new();
    let ctx = ctx();
    let _pending = begin(&name("phone"), Tier::View, false).expect("begin");
    for body in [
        "",
        "not json",
        "{}",
        r#"{"code":7}"#,
        r#"{"code":"ABCDEFG"}"#,
        r#"{"code":"ABCDEFGHJ"}"#,
        r#"{"code":"ABCDEFGU"}"#,
    ] {
        let resp = post_pair(&ctx, body);
        assert_eq!(
            (resp.status, body_json(&resp)),
            (
                400,
                serde_json::json!({"ok": false, "error": "bad_request"})
            ),
            "{body:?}"
        );
    }
    assert_eq!(
        live_pairing().expect("live").attempts_left,
        CODE_ATTEMPTS,
        "a malformed body costs nothing"
    );
}

// ── the wait ────────────────────────────────────────────────────────────────

#[test]
fn a_new_code_replaces_the_one_waiting() {
    let _home = HomeSandbox::new();
    let first = begin(&name("phone"), Tier::View, false).expect("first");
    let second = begin(&name("tablet"), Tier::Control, false).expect("second");
    let now = now_epoch_secs();
    assert_eq!(
        observe(&first, now).expect("observe"),
        Some(Outcome::Replaced)
    );
    assert_eq!(
        observe(&second, now).expect("observe"),
        None,
        "the new code is the live one"
    );
    withdraw(&first).expect("withdraw");
    assert!(
        live_pairing().is_some(),
        "withdrawing a replaced code leaves its replacement"
    );
    assert!(paired(&redeem(second.code()).expect("redeem")));
}

#[test]
fn the_wait_reads_each_ending() {
    {
        let _home = HomeSandbox::new();
        let pending = begin(&name("phone"), Tier::Control, false).expect("begin");
        redeem(pending.code()).expect("redeem");
        assert_eq!(
            observe(&pending, now_epoch_secs()).expect("observe"),
            Some(Outcome::Paired(Tier::Control))
        );
    }
    {
        let _home = HomeSandbox::new();
        let pending = begin(&name("phone"), Tier::View, false).expect("begin");
        for _ in 0..CODE_ATTEMPTS {
            redeem(&wrong_code(pending.code())).expect("redeem");
        }
        assert_eq!(
            observe(&pending, now_epoch_secs()).expect("observe"),
            Some(Outcome::Burned)
        );
    }
    {
        let _home = HomeSandbox::new();
        let now = now_epoch_secs();
        let pending =
            begin_at(&name("phone"), Tier::View, false, now - CODE_TTL_SECS).expect("begin");
        assert_eq!(
            observe(&pending, now).expect("observe"),
            Some(Outcome::Expired),
            "past its expiry and still on disk"
        );
        redeem_at(pending.code(), now).expect("redeem");
        assert_eq!(
            observe(&pending, now).expect("observe"),
            Some(Outcome::Expired),
            "past its expiry and deleted by the attempt that found it"
        );
    }
}

#[test]
fn the_wait_ends_when_another_thread_redeems() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let code = pending.code().clone();
    // Bounded: a redemption that never lands must fail the test, not park it
    // until the code expires 5 minutes later.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let waited = std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(50));
            redeem(&code).expect("redeem");
        });
        wait_for(
            &pending,
            || (std::time::Instant::now() > deadline).then_some(0),
            Duration::from_millis(5),
        )
        .expect("wait")
    });
    assert_eq!(waited, Waited::Done(Outcome::Paired(Tier::View)));
}

/// A signal ends the wait with the code still live; `finish` withdraws it and
/// the run exits `128 + signal`.
#[test]
fn a_signal_withdraws_the_code_and_exits_128_plus_the_signal() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let waited = wait_for(&pending, || Some(2), Duration::from_millis(5)).expect("wait");
    assert_eq!(waited, Waited::Interrupted(2));
    assert!(
        live_pairing().is_some(),
        "the wait leaves the code to finish"
    );

    let err = finish(&pending, Ok(waited)).expect_err("a signal ends the run");
    assert!(live_pairing().is_none(), "finish withdraws the code");
    assert_eq!(crate::exit_code(Err(err)), 130);
}

#[test]
fn every_other_ending_exits_one_with_its_reason() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    let failures = [Outcome::Replaced, Outcome::Burned, Outcome::Expired]
        .map(|outcome| finish(&pending, Ok(Waited::Done(outcome))).expect_err("not paired"));
    assert_eq!(
        failures.each_ref().map(ToString::to_string),
        [
            "a newer `clauth devices pair` replaced this code before anyone entered it",
            "the code was dropped after 5 wrong tries; run `clauth devices pair phone` for a new \
             one",
            "the code expired unused; run `clauth devices pair phone` for a new one",
        ]
    );
    assert!(
        live_pairing().is_none(),
        "an expired ending withdraws its code"
    );
    for err in failures {
        assert_eq!(crate::exit_code(Err(err)), 1);
    }
    finish(&pending, Ok(Waited::Done(Outcome::Paired(Tier::View)))).expect("paired exits 0");
}

/// A signal read after the code already paired reports the pairing, never a
/// withdrawal that did not happen: the wait looks for a signal before it looks
/// at the files, so a redemption can land between two polls.
#[test]
fn a_signal_after_the_code_paired_reports_the_pairing() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::Control, false).expect("begin");
    assert!(paired(&redeem(pending.code()).expect("redeem")));

    let result = finish(&pending, Ok(Waited::Interrupted(2)));

    assert_eq!(
        crate::exit_code(result),
        0,
        "the device paired, so the run did too"
    );
    assert!(
        devices::read_store()
            .expect("read")
            .named("phone")
            .is_some()
    );
}

/// The same race with a newer code: the signal reports the replacement, and
/// the newer code stays live for whoever minted it.
#[test]
fn a_signal_after_the_code_was_replaced_reports_the_replacement() {
    let _home = HomeSandbox::new();
    let first = begin(&name("phone"), Tier::View, false).expect("first");
    let _second = begin(&name("tablet"), Tier::View, false).expect("second");

    let Err(err) = finish(&first, Ok(Waited::Interrupted(2))) else {
        panic!("a replaced code is no pairing");
    };

    assert_eq!(
        err.to_string(),
        "a newer `clauth devices pair` replaced this code before anyone entered it"
    );
    assert!(live_pairing().is_some(), "the newer code stays live");
}

/// The redemption's last guard: a code whose name a device took meanwhile is
/// consumed and refused, never minting a second device under that name. `add`
/// and `pair` both refuse a waiting code's name, so only a hand edit reaches
/// this state; the test plants it directly.
#[test]
fn a_code_whose_name_was_taken_is_refused_and_consumed() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    devices::seed_for_tests(
        "phone",
        Tier::View,
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    )
    .expect("seed");
    let lines = crate::logline::LogLines::new();
    let _capture = lines.capture_here();

    assert!(!paired(&redeem(pending.code()).expect("redeem")));

    assert!(live_pairing().is_none(), "the code is consumed");
    assert_eq!(
        devices::read_store().expect("read").devices.len(),
        1,
        "no second device under the name"
    );
    assert_eq!(
        lines.snapshot(),
        vec![
            "clauth api: the pairing code for 'phone' was refused: a device took that name first"
                .to_string()
        ]
    );
}

/// A pairing file that does not parse redeems nothing and is deleted by the
/// first attempt that finds it, so the probes after it answer without the
/// flock.
#[test]
fn an_unparseable_pairing_file_is_deleted_by_the_attempt_that_finds_it() {
    let _home = HomeSandbox::new();
    let path = pairing_path().expect("path");
    crate::profile::mkdir_700(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, b"{ not json").expect("damage the pairing file");

    assert!(!paired(
        &redeem(&Code("ABCD2345".to_string())).expect("redeem")
    ));
    assert!(!path.exists(), "the attempt that found it deleted it");

    let before = crate::lock::OUTERMOST_ACQUISITIONS.with(std::cell::Cell::get);
    assert!(!paired(
        &redeem(&Code("ABCD2345".to_string())).expect("redeem")
    ));
    assert_eq!(
        crate::lock::OUTERMOST_ACQUISITIONS.with(std::cell::Cell::get),
        before,
        "the next probe takes no lock"
    );
}

/// A code that burned is not read as paired when an `add` takes its name before
/// the next poll: only a device that joined by pairing can be a code's outcome.
#[test]
fn a_burned_code_is_not_read_as_paired_by_a_later_add() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::View, false).expect("begin");
    for _ in 0..CODE_ATTEMPTS {
        redeem(&wrong_code(pending.code())).expect("redeem");
    }
    devices::add(&name("phone"), Tier::Control, false).expect("a burned code holds no name");

    assert_eq!(
        observe(&pending, now_epoch_secs()).expect("observe"),
        Some(Outcome::Burned)
    );
}

/// The lost-code fallback with a withdraw that fails pins its sentence by
/// exact words. Replacing the code file with a directory makes [`withdraw`]
/// fail its read on every platform; the read error and the sandbox path are
/// inputs to the sentence, so only those two are substituted, never the
/// approved copy.
#[test]
fn a_lost_code_line_with_a_failed_withdraw_pins_its_sentence() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("tray"), Tier::View, false).expect("mint the code");
    let path = pairing_path().expect("path");
    std::fs::remove_file(&path).expect("remove the code file");
    std::fs::create_dir(&path).expect("replace it with an unreadable entry");
    let err = withdraw_lost(&pending, None).expect_err("the withdraw read fails");
    let read_err = std::fs::read(&path).expect_err("a directory refuses a plain read");
    assert_eq!(
        err.to_string(),
        format!(
            "the pairing code for 'tray' never reached its reader and could not be withdrawn: \
             failed to read {}: {read_err}; it stays redeemable until it expires in 5 minutes",
            path.display()
        )
    );
}

/// The same arm with the write error present AND the withdrawal failing — the
/// double failure — pins its sentence by exact words: the parenthetical names
/// the write error, and the TTL is the operator's only remaining guarantee.
/// The dir-in-place-of-`pairing.json` seam makes [`withdraw`] fail its read on
/// every platform; the read error and the sandbox path are inputs to the
/// sentence, so only those two are substituted, never the approved copy.
#[test]
fn a_lost_code_line_with_a_write_error_and_a_failed_withdraw_pins_its_sentence() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("tray"), Tier::View, false).expect("mint the code");
    let path = pairing_path().expect("path");
    std::fs::remove_file(&path).expect("remove the code file");
    std::fs::create_dir(&path).expect("replace it with an unreadable entry");
    let write_err = std::io::Error::other("full disk");
    let err = withdraw_lost(&pending, Some(write_err)).expect_err("the withdraw read fails");
    let read_err = std::fs::read(&path).expect_err("a directory refuses a plain read");
    assert_eq!(
        err.to_string(),
        format!(
            "the pairing code for 'tray' never reached its reader (full disk) and could not be \
             withdrawn: failed to read {}: {read_err}; it stays redeemable until it expires in 5 minutes",
            path.display()
        )
    );
}

/// The lost-code fallback with a withdraw that lands pins its success-arm
/// sentence by exact words, so a reword of the approved copy cannot ship green.
#[test]
fn a_lost_code_line_with_a_withdrawn_code_pins_its_sentence() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("tray"), Tier::View, false).expect("mint the code");
    let err = withdraw_lost(&pending, None).expect_err("the withdraw lands");
    assert_eq!(
        err.to_string(),
        "the pairing code for 'tray' never reached its reader; it was withdrawn"
    );
}

/// The same arm under a write error renders the io error's Display in a
/// parenthetical, pinned exactly against the real message the helper produces.
#[test]
fn a_lost_code_line_with_a_write_error_pins_the_cause_and_withdrawal() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("tray"), Tier::View, false).expect("mint the code");
    let write_err = std::io::Error::other("full disk");
    let err = withdraw_lost(&pending, Some(write_err)).expect_err("the withdraw lands");
    assert_eq!(
        err.to_string(),
        "the pairing code for 'tray' never reached its reader (full disk); it was withdrawn"
    );
}

/// `Ok(false)` from `withdraw` means a newer `pair` replaced the code before
/// anyone read it; the message says replaced, not withdrawn, by exact words.
#[test]
fn a_lost_code_line_on_a_replaced_code_says_replaced() {
    let _home = HomeSandbox::new();
    let first = begin(&name("tray"), Tier::View, false).expect("first code");
    begin(&name("tray"), Tier::View, false).expect("a newer pair replaces it");
    let err = withdraw_lost(&first, None).expect_err("the code is already gone");
    assert_eq!(
        err.to_string(),
        "the pairing code for 'tray' never reached its reader; a newer `clauth devices pair` had already replaced it"
    );
}

/// The replaced sentence under a write error still renders the parenthetical,
/// pinned exactly against the real message the helper produces.
#[test]
fn a_lost_code_line_with_a_write_error_and_a_replaced_code_pins_its_sentence() {
    let _home = HomeSandbox::new();
    let first = begin(&name("tray"), Tier::View, false).expect("first code");
    begin(&name("tray"), Tier::View, false).expect("a newer pair replaces it");
    let write_err = std::io::Error::other("full disk");
    let err = withdraw_lost(&first, Some(write_err)).expect_err("the code is already gone");
    assert_eq!(
        err.to_string(),
        "the pairing code for 'tray' never reached its reader (full disk); a newer `clauth devices pair` had already replaced it"
    );
}

// ── sessions grant ─────────────────────────────────────────────────────────

/// A code minted with the grant carries it into the redeemed device; a code
/// minted without it redeems to a device with the grant off.
#[test]
fn a_pairing_code_carries_the_sessions_grant() {
    for sessions in [false, true] {
        let _home = HomeSandbox::new();
        let pending = begin(&name("phone"), Tier::Control, sessions).expect("begin");
        let Redeemed::Paired { token, .. } = redeem(pending.code()).expect("redeem") else {
            panic!("the right code must pair");
        };
        let device = devices::authenticate(Some(&token))
            .expect("read")
            .expect("the minted token verifies");
        assert_eq!(device.sessions, sessions, "the grant rides the pairing");
        assert!(live_pairing().is_none(), "a redeemed code is gone");
    }
}

/// A pending file written by a build before the sessions field existed (no
/// key) redeems to an ungranted device, not a refused pairing.
#[test]
fn a_pre_sessions_pairing_file_redeems_ungranted() {
    let _home = HomeSandbox::new();
    let pending = begin(&name("phone"), Tier::Control, true).expect("begin");
    let path = pairing_path().expect("path");
    let mut body: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
    body.as_object_mut().expect("object").remove("sessions");
    std::fs::write(&path, body.to_string()).expect("strip the key");

    let Redeemed::Paired { token, .. } = redeem(pending.code()).expect("redeem") else {
        panic!("a pre-field pending file must still pair");
    };
    let device = devices::authenticate(Some(&token))
        .expect("read")
        .expect("the minted token verifies");
    assert!(!device.sessions, "a missing key reads as the grant off");
}
