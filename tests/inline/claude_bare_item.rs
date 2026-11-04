//! The bare Keychain item's rescue rules over real profile stores: the writer
//! guard, the carry into an owner's store, the pre-flock settle every switch,
//! sign-out and guarded relink runs, and the daemon leg, driven with a fake
//! `/profile` probe and a recording restore. The `security` glue around them
//! is macOS-only; these run on every platform.

use super::*;
use crate::profile::{AccountId, ProfileName, save_profile};
use crate::testutil::HomeSandbox;
use std::cell::RefCell;
use std::fs;

/// Every fixture login expires this far into the future (epoch ms, 2096), so
/// only a test that asks for an expired item meets one; `expires` keeps the
/// fixtures' relative order.
const FUTURE_MS: i64 = 4_000_000_000_000;

fn pair(access: &str, refresh: &str, expires: i64) -> serde_json::Value {
    serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": FUTURE_MS + expires,
        }
    })
}

/// A login whose access token expired a minute ago.
fn expired_pair(access: &str, refresh: &str) -> serde_json::Value {
    let past = i64::try_from(crate::usage::now_ms()).expect("epoch ms fits i64") - 60_000;
    serde_json::json!({
        "claudeAiOauth": { "accessToken": access, "refreshToken": refresh, "expiresAt": past }
    })
}

fn store(name: &str) -> std::path::PathBuf {
    profile_dir(&ProfileName::from(name))
        .expect("profile dir")
        .join("credentials.json")
}

/// The carry into `name`'s store, on the account `name` holds now.
fn carry(name: &str, item: &serde_json::Value) -> anyhow::Result<BareItemCarry> {
    carry_bare_item_login_into(
        &ProfileName::from(name),
        &crate::testutil::profile_id(name),
        item,
    )
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).expect("read")).expect("parse")
}

/// Save `name` holding `login` in its `credentials.json`, with `account`
/// cached as its account id when given.
fn profile_holding(name: &str, login: &serde_json::Value, account: Option<&str>) {
    save_profile(&crate::profile::Profile::new(name.to_string(), None, None)).expect("save");
    fs::write(store(name), serde_json::to_vec_pretty(login).expect("ser")).expect("write store");
    if let Some(account) = account {
        // Straight to the file: the cache writer skips a name the roster,
        // written after the profiles, does not carry yet.
        fs::write(
            store(name).with_file_name(crate::profile_cache::ACCOUNT_ID_CACHE_FILE),
            serde_json::to_vec(&AccountId::from(account)).expect("ser"),
        )
        .expect("write account id");
    }
}

/// Save `name` with no login of its own: an api-key or third-party profile.
fn keyless_profile(name: &str) {
    save_profile(&crate::profile::Profile::new(name.to_string(), None, None)).expect("save");
}

fn roster(names: &[&str], active: &str) {
    let state = crate::profile::AppState {
        active_profile: Some(active.into()),
        profiles: names.iter().map(|n| ProfileName::from(*n)).collect(),
        ..Default::default()
    };
    crate::profile::save_app_state(&state).expect("persist state");
}

fn spend(name: &str, refresh: &str) {
    let record = vec![crate::codex_auth::token_fingerprint(refresh)];
    fs::write(
        store(name).with_file_name("auth.spent.json"),
        serde_json::to_vec(&record).expect("ser"),
    )
    .expect("write spent record");
}

/// A live rotation stage beside `name`'s store (`oauth::STAGE_FILE`).
fn stage(name: &str) {
    fs::write(store(name).with_file_name("credentials.json.staged"), b"{}").expect("stage");
}

fn sha256(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

const LEFT: &str =
    "clauth: the macOS Keychain holds a login no clauth account claims; left it in place";

/// What a [`correct_bare_item`] run did: the restore's `placed` argument per
/// call, the probe's token per call, and the lines it raised.
struct Leg {
    restores: Vec<Option<[u8; 32]>>,
    probes: Vec<String>,
    lines: Vec<String>,
}

/// One leg run: `probed` is the `/profile` answer (`Err` carries the cause),
/// `restored` what every restore call reports.
fn run_leg_with(
    item: &serde_json::Value,
    active: Option<&str>,
    probed: Result<&str, &str>,
    restored: bool,
) -> Leg {
    let restores = RefCell::new(Vec::new());
    let probes = RefCell::new(Vec::new());
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let active = active.map(ProfileName::from);
    correct_bare_item(
        item,
        active.as_ref(),
        |token| {
            probes.borrow_mut().push(token.to_string());
            probed
                .map(AccountId::from)
                .map_err(|cause| anyhow::anyhow!("{cause}"))
        },
        |placed| {
            restores.borrow_mut().push(placed.copied());
            Ok(restored)
        },
    );
    drop(capture);
    Leg {
        restores: restores.into_inner(),
        probes: probes.into_inner(),
        lines: lines.snapshot(),
    }
}

/// A leg run whose probe answers `account`, or fails when `None`.
fn run_leg(item: &serde_json::Value, active: &str, account: Option<&str>) -> Leg {
    run_leg_with(item, Some(active), account.ok_or("no answer"), true)
}

fn line(text: &str) -> Vec<String> {
    vec![format!("clauth: {text}")]
}

// ── the writer guard ───────────────────────────────────────────────────────

#[test]
fn a_writer_leaves_an_item_no_store_saved_and_says_so() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), None);
    roster(&["a"], "a");
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let replaceable = bare_item_may_be_replaced(&pair("succ-acc", "succ-ref", 2000), None);
    drop(capture);
    assert!(!replaceable);
    assert_eq!(lines.snapshot(), vec![LEFT.to_string()]);
}

#[test]
fn a_writer_replaces_an_item_that_strands_nothing() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), None);
    profile_holding("b", &pair("b-acc", "b-ref", 1000), None);
    roster(&["a", "b"], "a");
    spend("b", "spent-ref");
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    assert!(
        bare_item_may_be_replaced(&pair("newer-acc", "b-ref", 5000), None),
        "a store holds its refresh token"
    );
    assert!(
        bare_item_may_be_replaced(&pair("x-acc", "spent-ref", 5000), None),
        "the server consumed its refresh token"
    );
    assert!(
        bare_item_may_be_replaced(
            &pair("placed-acc", "placed-ref", 5000),
            Some(&sha256("placed-acc"))
        ),
        "the daemon leg settled it"
    );
    assert!(
        bare_item_may_be_replaced(
            &serde_json::json!({ "claudeAiOauth": { "accessToken": "mint-acc" } }),
            None
        ),
        "no refresh token to strand"
    );
    assert!(bare_item_may_be_replaced(
        &serde_json::json!({ "mcpOAuth": {} }),
        None
    ));
    drop(capture);
    assert!(lines.snapshot().is_empty(), "{:?}", lines.snapshot());
}

/// Every real writer of the bare item runs the guard over the bytes it
/// replaces, and every switch, sign-out and guarded-relink entry settles the
/// item before it takes the state flock: the macOS glue cannot build here,
/// so dropping either ships green on every platform without this pin.
#[test]
fn every_bare_item_writer_runs_the_guard_and_settles_first() {
    let src = include_str!("../../src/claude.rs");
    let body = |start: &str| {
        let from = src.split_once(start).expect("fn defined").1;
        from.split_once("\n}\n")
            .expect("fn body closes")
            .0
            .to_string()
    };
    let mirror = body("fn keychain_mirror_source(");
    assert!(
        mirror.contains("bare_item_may_be_replaced(item, placed)"),
        "the relink's install and sign-out guard the item they replace"
    );
    assert!(
        body("pub(crate) fn clear_claude_credentials(")
            .contains("bare_item_may_be_replaced(item, None)"),
        "the wrap-off sign-out guards the item it deletes"
    );
    for (file, text) in [
        ("src/actions.rs", include_str!("../../src/actions.rs")),
        ("src/main.rs", include_str!("../../src/main.rs")),
        ("src/tui/app.rs", include_str!("../../src/tui/app.rs")),
    ] {
        assert!(
            text.contains("settle_bare_keychain_item("),
            "{file} settles the bare item before a switch, sign-out or relink"
        );
    }
    for writer in [
        "pub(crate) fn force_link_profile_credentials(",
        "pub(crate) fn link_profile_credentials(",
    ] {
        assert!(
            body(writer).contains("== ItemWrite::Refused"),
            "{writer} fails before it publishes the link when its guard refuses"
        );
    }
}

/// The body of the item at `start` in `text`, up to its closing brace at
/// `close` (`"\n}\n"` for a free fn, `"\n    }\n"` for a method).
fn item_body<'a>(text: &'a str, start: &str, close: &str) -> &'a str {
    let from = text.split_once(start).expect("item defined").1;
    from.split_once(close).expect("item body closes").0
}

/// Where `needle` first sits in `body`, failing with the entry's name.
fn at(body: &str, needle: &str, entry: &str) -> usize {
    body.find(needle)
        .unwrap_or_else(|| panic!("{entry} calls {needle}"))
}

/// Every entry that settles the bare item does so ABOVE its AUTH-1 gate or
/// rolling arm: when the switch targets the item's owner, the gate refreshes
/// the owner's consumed predecessor, gets `invalid_grant` and quarantines it,
/// so a settle below the gate never runs. The daemon's two
/// drains settle without blocking the run loop on HTTP; `boot` runs
/// before the watchdog and settles in full.
#[test]
fn every_settle_runs_above_its_gate_and_no_drain_blocks_on_a_probe() {
    const SETTLE: &str = "settle_bare_keychain_item(";
    const OFF_LOOP: &str = "settle_bare_keychain_item_off_loop(";
    let actions = include_str!("../../src/actions.rs");
    let main = include_str!("../../src/main.rs");
    let app = include_str!("../../src/tui/app.rs");
    let tick = include_str!("../../src/daemon/tick.rs");
    let daemon = include_str!("../../src/daemon/mod.rs");
    for (entry, text, gate) in [
        (
            "pub(crate) fn switch_profile_cli(",
            actions,
            "ensure_installable(",
        ),
        (
            "pub(crate) fn switch_profile_noninteractive(",
            actions,
            "ensure_installable(",
        ),
        // The rolling arm lives in the shared arm half; both of its callers
        // settle the bare item above the arm-half call, which keeps the
        // settle above the arm on every path (the CLI's and the TUI
        // worker's).
        ("fn cmd_rolling_token(", main, "arm_rolling_token("),
        ("fn start_rolling_switch(", app, "arm_rolling_token("),
        ("fn spawn_switch_gate<", app, "ensure_installable("),
    ] {
        let body = item_body(text, entry, "\n}\n");
        assert!(
            at(body, SETTLE, entry) < at(body, gate, entry),
            "{entry} settles the bare item above its {gate}"
        );
    }
    // The pre-arm half runs before the settle in both rolling callers, so a
    // refused switch settles nothing.
    for (entry, text) in [
        ("fn cmd_rolling_token(", main),
        ("fn start_rolling_switch(", app),
    ] {
        let body = item_body(text, entry, "\n}\n");
        assert!(
            at(body, "precheck_rolling_token(", entry) < at(body, SETTLE, entry),
            "{entry} refuses the switch before it settles"
        );
    }
    assert!(
        item_body(actions, "pub(crate) fn arm_rolling_token(", "\n}\n")
            .contains("crate::oauth::arm_rolling_token("),
        "the shared arm half owns the rolling arm"
    );
    for entry in [
        "pub(super) fn drain_pending_switch(&mut self)",
        "pub(super) fn drain_pending_switch_off(&mut self)",
    ] {
        let body = item_body(tick, entry, "\n    }\n");
        assert!(
            !body.contains(SETTLE),
            "{entry} never probes `/profile` on the run loop"
        );
        assert!(body.contains(OFF_LOOP), "{entry} settles off the loop");
    }
    let drain = item_body(
        tick,
        "pub(super) fn drain_pending_switch(&mut self)",
        "\n    }\n",
    );
    assert!(
        at(drain, OFF_LOOP, "drain_pending_switch")
            < at(
                drain,
                "ensure_installable_from_main_loop(",
                "drain_pending_switch"
            ),
        "the drain settles above its AUTH-1 gate"
    );
    assert!(
        item_body(daemon, "    fn boot(&self) {", "\n    }\n").contains(SETTLE),
        "the daemon settles the bare item at boot, before its first relink"
    );
}

/// An entry's settled placement reaches every writer guard on its thread
/// while it lives, and no further.
#[test]
fn a_writer_replaces_the_item_its_entry_settled_and_only_inside_the_entry() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), None);
    roster(&["a"], "a");
    let item = pair("placed-acc", "placed-ref", 5000);
    {
        let _placed = BareItemPlacement::enter(Some(sha256("placed-acc")));
        assert!(bare_item_may_be_replaced(&item, None));
    }
    let lines = crate::logline::LogLines::new();
    let capture = lines.capture_here();
    let after = bare_item_may_be_replaced(&item, None);
    drop(capture);
    assert!(!after, "the placement ends with its scope");
    assert_eq!(lines.snapshot(), vec![LEFT.to_string()]);
}

// ── the carry into an owner's store ────────────────────────────────────────

#[test]
fn the_carry_saves_a_later_login_into_the_store() {
    let _home = HomeSandbox::new();
    profile_holding("b", &pair("b-acc", "b-ref", 1000), None);
    let item = pair("succ-acc", "succ-ref", 2000);
    assert_eq!(carry("b", &item).expect("carry"), BareItemCarry::Saved);
    assert_eq!(read_json(&store("b")), item);
}

/// The item is whatever account's Claude Code last wrote it, so the keys
/// beside its login (MCP logins, identity keys) may be another account's:
/// the carry takes the login and keeps the store's own keys.
#[test]
fn the_carry_takes_the_login_alone() {
    let _home = HomeSandbox::new();
    let own_mcp = serde_json::json!({ "b-server": { "accessToken": "b-mcp" } });
    let mut held = pair("b-acc", "b-ref", 1000);
    held["mcpOAuth"] = own_mcp.clone();
    profile_holding("b", &held, None);
    let mut item = pair("succ-acc", "succ-ref", 2000);
    item["mcpOAuth"] = serde_json::json!({ "a-server": { "accessToken": "a-mcp" } });
    item["organizationUuid"] = serde_json::json!("org-of-a");
    assert_eq!(carry("b", &item).expect("carry"), BareItemCarry::Saved);
    let saved = read_json(&store("b"));
    assert_eq!(saved["claudeAiOauth"]["accessToken"], "succ-acc");
    assert_eq!(saved["claudeAiOauth"]["refreshToken"], "succ-ref");
    assert_eq!(saved["mcpOAuth"], own_mcp);
    assert!(saved.get("organizationUuid").is_none(), "{saved}");
}

#[test]
fn the_carry_keeps_a_store_that_outranks_or_outlived_the_item() {
    let _home = HomeSandbox::new();
    profile_holding("later", &pair("l-acc", "l-ref", 3000), None);
    profile_holding("spent", &pair("s-acc", "s-ref", 1000), None);
    spend("spent", "succ-ref");
    let item = pair("succ-acc", "succ-ref", 2000);
    for name in ["later", "spent"] {
        let before = fs::read(store(name)).expect("read");
        assert_eq!(
            carry(name, &item).expect("carry"),
            BareItemCarry::Kept,
            "{name}"
        );
        assert_eq!(fs::read(store(name)).expect("read"), before, "{name}");
    }
}

#[test]
fn the_carry_refuses_what_it_cannot_judge() {
    let _home = HomeSandbox::new();
    profile_holding("unread", &pair("u-acc", "u-ref", 1000), None);
    fs::write(
        store("unread").with_file_name("auth.spent.json"),
        b"not json",
    )
    .expect("write record");
    profile_holding("pending", &pair("p-acc", "p-ref", 1000), None);
    fs::write(
        store("pending").with_file_name("credentials.json.pending"),
        b"{}",
    )
    .expect("stage");
    // A live stage's minted pair is lost once its base changes under it.
    profile_holding("staged", &pair("st-acc", "st-ref", 1000), None);
    stage("staged");
    let item = pair("succ-acc", "succ-ref", 2000);
    for name in ["unread", "pending", "staged"] {
        let before = fs::read(store(name)).expect("read");
        assert_eq!(
            carry(name, &item).expect("carry"),
            BareItemCarry::Refused,
            "{name}"
        );
        assert_eq!(fs::read(store(name)).expect("read"), before, "{name}");
    }
    let mint = serde_json::json!({ "claudeAiOauth": { "accessToken": "m", "expiresAt": 9000 } });
    assert_eq!(
        carry("pending", &mint).expect("carry"),
        BareItemCarry::Refused,
        "a login with no refresh token is no rotating pair"
    );
}

// ── the pre-flock settle (switch, sign-out, guarded relink) ────────────────

fn settle(
    item: &serde_json::Value,
    writer: BareItemWriter,
    probed: Result<&str, &str>,
) -> Result<BareItemSettle> {
    settle_bare_item_before_write(item, writer, |_| {
        probed
            .map(AccountId::from)
            .map_err(|cause| anyhow::anyhow!("{cause}"))
    })
}

fn settled(
    notice: Option<&str>,
    saved_into: Option<&str>,
    placed: Option<[u8; 32]>,
) -> BareItemSettle {
    BareItemSettle::Settled(BareItemSettled {
        notice: notice.map(str::to_string),
        saved_into: saved_into.map(ProfileName::from),
        placed,
    })
}

#[test]
fn a_switch_saves_a_siblings_unsaved_login_before_it_writes() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let item = pair("succ-acc", "succ-ref", 2000);
    let settled = settle(&item, BareItemWriter::Switch, Ok("uuid-b")).expect("settle");
    assert_eq!(read_json(&store("b")), item);
    assert_eq!(
        settled,
        self::settled(
            Some("saved the Keychain login Claude Code wrote into b"),
            Some("b"),
            None
        )
    );
}

#[test]
fn a_switch_over_a_saved_login_settles_without_a_probe() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    roster(&["a"], "a");
    let settled = settle_bare_item_before_write(
        &pair("a-acc", "a-ref", 1000),
        BareItemWriter::Switch,
        |_| panic!("a saved login needs no probe"),
    )
    .expect("settle");
    assert_eq!(settled, self::settled(None, None, None));
}

#[test]
fn a_switch_whose_lookup_fails_refuses_and_writes_nothing() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    roster(&["a"], "a");
    let before = fs::read(store("a")).expect("read");
    let item = pair("succ-acc", "succ-ref", 2000);
    for (writer, verb) in [
        (BareItemWriter::Switch, "switching"),
        (BareItemWriter::SignOut, "signing out"),
    ] {
        let err = settle(&item, writer, Err("connection refused")).expect_err("refused");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "the macOS Keychain holds a login no clauth account has saved yet, and clauth \
                 could not look up its owner (connection refused); not {verb}, so that login \
                 is not lost; run the switch again once online"
            )
        );
    }
    assert_eq!(fs::read(store("a")).expect("read"), before);
}

/// A settle that cannot place the item's login asks instead of overwriting
/// it: the caller shows the question on a
/// terminal or in the TUI, and refuses where nobody can answer. Every case
/// writes nothing.
#[test]
fn a_switch_asks_when_it_cannot_place_the_items_login() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-shared"));
    profile_holding("c", &pair("c-acc", "c-ref", 1000), Some("uuid-shared"));
    roster(&["a", "b", "c"], "a");
    let item = pair("stranger-acc", "stranger-ref", 2000);
    let stores = || ["a", "b", "c"].map(|n| fs::read(store(n)).expect("read"));
    let before = stores();
    assert_eq!(
        settle(&item, BareItemWriter::Switch, Ok("uuid-nobody")).expect("settle"),
        BareItemSettle::NeedsChoice(BareItemUnplaced::UnknownAccount),
        "every login-holding profile is cached and none is the item's account"
    );
    assert_eq!(
        settle(&item, BareItemWriter::Switch, Ok("uuid-shared")).expect("settle"),
        BareItemSettle::NeedsChoice(BareItemUnplaced::SharedBy("b".into(), "c".into())),
        "two profiles share the item's account"
    );
    assert_eq!(stores(), before);
}

/// The owner's account id was never cached (a force snapshot removed it, or
/// its login probe failed): the item may be that profile's own successor, so
/// replacing it could kill the account.
#[test]
fn a_switch_asks_while_a_login_holding_profile_has_no_cached_id() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), None);
    keyless_profile("k");
    roster(&["a", "b", "k"], "a");
    let before = fs::read(store("b")).expect("read");
    assert_eq!(
        settle_bare_item_before_write(
            &pair("succ-acc", "succ-ref", 2000),
            BareItemWriter::SignOut,
            |token| match token {
                "b-acc" => Err(anyhow::anyhow!("connection refused")),
                _ => Ok(AccountId::from("uuid-b")),
            }
        )
        .expect("settle"),
        BareItemSettle::NeedsChoice(BareItemUnplaced::NotCached)
    );
    assert_eq!(fs::read(store("b")).expect("read"), before);
}

/// The uncached profile's own unexpired token names its account: once that
/// id is cached, the item's login places into it and the writer goes on.
#[test]
fn a_switch_looks_an_uncached_profiles_account_up_and_saves_into_it() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), None);
    roster(&["a", "b"], "a");
    let item = pair("succ-acc", "succ-ref", 2000);
    let settled = settle(&item, BareItemWriter::Switch, Ok("uuid-b")).expect("settle");
    assert_eq!(read_json(&store("b")), item);
    assert_eq!(
        settled,
        self::settled(
            Some("saved the Keychain login Claude Code wrote into b"),
            Some("b"),
            None
        )
    );
}

/// A keyless profile holds no login, so its missing id names no candidate
/// owner: an id no login-holding profile is cached with is a stranger's.
#[test]
fn a_keyless_profiles_missing_id_leaves_a_stranger_unknown() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    keyless_profile("k");
    roster(&["a", "k"], "a");
    assert_eq!(
        settle(
            &pair("stranger-acc", "stranger-ref", 2000),
            BareItemWriter::Switch,
            Ok("uuid-nobody")
        )
        .expect("settle"),
        BareItemSettle::NeedsChoice(BareItemUnplaced::UnknownAccount)
    );
}

/// `/profile` answers 401 for an expired access token for good, so the settle
/// asks without spending a probe.
#[test]
fn a_switch_over_an_expired_unsaved_login_asks_without_a_probe() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    roster(&["a"], "a");
    let before = fs::read(store("a")).expect("read");
    let asked = settle_bare_item_before_write(
        &expired_pair("old-acc", "old-ref"),
        BareItemWriter::Switch,
        |_| panic!("an expired access token is never probed"),
    )
    .expect("settle");
    assert_eq!(
        asked,
        BareItemSettle::NeedsChoice(BareItemUnplaced::Expired)
    );
    assert_eq!(fs::read(store("a")).expect("read"), before);
}

/// An expired login a store already holds, or whose refresh token the server
/// consumed, strands nothing: the settle passes it as before.
#[test]
fn an_expired_login_that_strands_nothing_settles_as_before() {
    let _home = HomeSandbox::new();
    profile_holding("a", &expired_pair("a-acc", "a-ref"), Some("uuid-a"));
    spend("a", "dead-ref");
    roster(&["a"], "a");
    for item in [
        expired_pair("a-acc", "a-ref"),
        expired_pair("dead-acc", "dead-ref"),
    ] {
        assert_eq!(
            settle_bare_item_before_write(&item, BareItemWriter::Switch, |_| panic!("no probe"))
                .expect("settle"),
            settled(None, None, None)
        );
    }
}

/// The question and the refusal, per reason, by equality.
#[test]
fn the_unplaced_question_and_refusal_name_their_reason() {
    for (unplaced, reason) in [
        (
            BareItemUnplaced::NotCached,
            "its account's id is not cached yet",
        ),
        (
            BareItemUnplaced::SharedBy("p1".into(), "p2".into()),
            "two profiles share its account (p1, p2)",
        ),
        (
            BareItemUnplaced::Expired,
            "its access token expired or was revoked",
        ),
        (
            BareItemUnplaced::UnknownAccount,
            "no clauth account is logged in to its account",
        ),
    ] {
        assert_eq!(unplaced.reason(), reason);
        assert_eq!(
            unplaced.question(),
            format!(
                "the macOS Keychain holds a login no clauth account has saved ({reason}); what \
                 should clauth do with it?"
            )
        );
        for (writer, verb) in [
            (BareItemWriter::Switch, "switching"),
            (BareItemWriter::SignOut, "signing out"),
        ] {
            assert_eq!(
                format!("{:#}", unplaced.refusal(writer)),
                format!(
                    "the macOS Keychain holds a login no clauth account has saved ({reason}); \
                     not {verb}, so that login is not lost; save it first with:  clauth \
                     capture <name>"
                )
            );
        }
    }
}

/// What `clauth capture` and the prompt's `save` take on macOS: the bare
/// item's login when no store holds it and the server has not consumed it,
/// else nothing (the capture then reads the credentials file as before).
#[test]
fn capture_takes_the_items_login_only_while_no_store_saved_it() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    spend("a", "dead-ref");
    roster(&["a"], "a");
    let unsaved = pair("succ-acc", "succ-ref", 2000);
    let login = unsaved_bare_item_login(&unsaved).expect("an unsaved login is captured");
    assert_eq!(login.access_token(), Some("succ-acc"));
    assert_eq!(login.refresh_token(), Some("succ-ref"));
    let expired = expired_pair("old-acc", "old-ref");
    assert_eq!(
        unsaved_bare_item_login(&expired).and_then(|l| l.refresh_token().map(str::to_string)),
        Some("old-ref".to_string()),
        "an expired login's refresh token still saves the account"
    );
    for (item, why) in [
        (pair("a-acc", "a-ref", 1000), "a store holds it"),
        (pair("dead-acc", "dead-ref", 2000), "the server consumed it"),
        (
            serde_json::json!({ "claudeAiOauth": { "accessToken": "mint-acc" } }),
            "no refresh token",
        ),
        (serde_json::json!({ "mcpOAuth": {} }), "no login"),
    ] {
        assert!(unsaved_bare_item_login(&item).is_none(), "{why}");
    }
}

#[test]
fn a_switch_refuses_while_the_owners_store_cannot_take_the_login() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    stage("b");
    let before = fs::read(store("b")).expect("read");
    let err = settle(
        &pair("succ-acc", "succ-ref", 2000),
        BareItemWriter::Switch,
        Ok("uuid-b"),
    )
    .expect_err("refused");
    assert_eq!(
        format!("{err:#}"),
        "the macOS Keychain holds b's login, but b's saved login is mid-rotation or \
         unreadable; not switching, so that login is not lost; run the switch again in a minute"
    );
    assert_eq!(fs::read(store("b")).expect("read"), before);
}

/// The owner already holds a later login: the item's older pair strands
/// nothing, so the writer may replace it, with no notice.
#[test]
fn a_switch_over_a_login_its_owner_outlived_hands_the_guard_its_key() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 9000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let before = fs::read(store("b")).expect("read");
    let settled = settle(
        &pair("older-acc", "older-ref", 2000),
        BareItemWriter::Switch,
        Ok("uuid-b"),
    )
    .expect("settle");
    assert_eq!(
        settled,
        self::settled(None, None, Some(sha256("older-acc")))
    );
    assert_eq!(fs::read(store("b")).expect("read"), before);
}

// ── the daemon leg ─────────────────────────────────────────────────────────

#[test]
fn the_leg_saves_a_siblings_successor_then_puts_the_active_login_back() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let item = pair("rescue-acc", "rescue-ref", 2000);
    let leg = run_leg(&item, "a", Some("uuid-b"));
    assert_eq!(leg.probes, vec!["rescue-acc".to_string()]);
    assert_eq!(read_json(&store("b")), item);
    assert_eq!(leg.restores, vec![None]);
    assert_eq!(
        leg.lines,
        line("saved the Keychain login Claude Code wrote into b and put a back")
    );
}

/// With a keyless active profile, or none, the leg still saves the item's
/// login into its owner; the restore then signs the item out.
#[test]
fn the_leg_saves_into_the_owner_whatever_the_active_profile_holds() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    keyless_profile("key");
    roster(&["a", "key"], "key");
    let item = pair("keyless-acc", "keyless-ref", 2000);
    let keyless = run_leg_with(&item, Some("key"), Ok("uuid-a"), true);
    assert_eq!(read_json(&store("a")), item);
    assert_eq!(keyless.restores, vec![None]);
    assert_eq!(
        keyless.lines,
        line(
            "saved the Keychain login Claude Code wrote into a and signed the Keychain out, \
             since key has no login to put back"
        )
    );

    let next = pair("next-acc", "next-ref", 3000);
    let none = run_leg_with(&next, None, Ok("uuid-a"), true);
    assert_eq!(read_json(&store("a")), next);
    assert_eq!(none.restores, vec![None]);
    assert_eq!(
        none.lines,
        line(
            "saved the Keychain login Claude Code wrote into a and signed the Keychain out, \
             since no clauth account is active"
        )
    );
}

#[test]
fn the_leg_puts_the_active_login_back_over_a_sibling_login_a_store_holds() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), None);
    profile_holding("b", &pair("b-acc", "b-ref", 1000), None);
    roster(&["a", "b"], "a");
    let leg = run_leg(&pair("b-acc", "b-ref", 1000), "a", None);
    assert!(leg.probes.is_empty(), "token equality needs no probe");
    assert_eq!(leg.restores, vec![None]);
    assert_eq!(
        leg.lines,
        line(
            "the macOS Keychain held b's login, which b already has saved or has replaced \
             with a newer one; put a back"
        )
    );
}

/// A sibling's mint (an access token alone, in `session-token.json`) is a
/// saved login too.
#[test]
fn the_leg_puts_the_active_login_back_over_a_siblings_mint() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), None);
    profile_holding("b", &pair("b-acc", "b-ref", 1000), None);
    roster(&["a", "b"], "a");
    let mint = serde_json::json!({ "claudeAiOauth": { "accessToken": "mint-b" } });
    fs::write(
        store("b").with_file_name("session-token.json"),
        serde_json::to_vec(&mint).expect("ser"),
    )
    .expect("write mint");
    let leg = run_leg(&mint, "a", None);
    assert!(leg.probes.is_empty(), "token equality needs no probe");
    assert_eq!(leg.restores, vec![None]);
    assert_eq!(
        leg.lines,
        line(
            "the macOS Keychain held b's login, which b already has saved or has replaced \
             with a newer one; put a back"
        )
    );
}

#[test]
fn the_leg_adopts_the_active_accounts_successor_and_restores_nothing() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let item = pair("adopt-acc", "adopt-ref", 2000);
    let leg = run_leg(&item, "a", Some("uuid-a"));
    assert_eq!(read_json(&store("a")), item);
    assert!(leg.restores.is_empty(), "{:?}", leg.restores);
    assert_eq!(
        leg.lines,
        line("the macOS Keychain holds a's own login, refreshed by Claude Code; saved it into a")
    );
}

/// The item holds an older login of the active account: its store keeps the
/// newer one, which goes back.
#[test]
fn the_leg_puts_the_active_login_back_over_an_older_one_of_its_own() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 9000), Some("uuid-a"));
    roster(&["a"], "a");
    let before = fs::read(store("a")).expect("read");
    let leg = run_leg(
        &pair("own-older-acc", "own-older-ref", 2000),
        "a",
        Some("uuid-a"),
    );
    assert_eq!(fs::read(store("a")).expect("read"), before);
    assert_eq!(leg.restores, vec![Some(sha256("own-older-acc"))]);
    assert_eq!(
        leg.lines,
        line("the macOS Keychain held an older login of a's; put its current one back")
    );
}

/// A sibling store that already holds a later login keeps it; the item's
/// older pair is then no account's only live one, so the active login goes
/// back with the item's key handed to the writer guard.
#[test]
fn the_leg_restores_over_an_item_its_owner_outlived() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 9000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let before = fs::read(store("b")).expect("read");
    let leg = run_leg(&pair("older-acc", "older-ref", 2000), "a", Some("uuid-b"));
    assert_eq!(fs::read(store("b")).expect("read"), before);
    assert_eq!(leg.restores, vec![Some(sha256("older-acc"))]);
    assert_eq!(
        leg.lines,
        line(
            "the macOS Keychain held b's login, which b already has saved or has replaced \
             with a newer one; put a back"
        )
    );
}

/// A restore that did not land (a switch moved the active profile meanwhile)
/// still spends the token's probe for the window.
#[test]
fn a_restore_that_did_not_land_still_spends_the_probe() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 9000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let item = pair("unlanded-acc", "unlanded-ref", 2000);
    let first = run_leg_with(&item, Some("a"), Ok("uuid-b"), false);
    assert_eq!(first.probes, vec!["unlanded-acc".to_string()]);
    let again = run_leg_with(&item, Some("a"), Ok("uuid-b"), false);
    assert!(again.probes.is_empty(), "one probe per token per window");
}

#[test]
fn the_leg_writes_nothing_without_one_owner_and_probes_a_token_once() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    let stores_before = (
        fs::read(store("a")).expect("read"),
        fs::read(store("b")).expect("read"),
    );
    let item = pair("orphan-acc", "orphan-ref", 2000);
    let leg = run_leg(&item, "a", Some("uuid-nobody"));
    assert_eq!(leg.probes, vec!["orphan-acc".to_string()]);
    assert!(leg.restores.is_empty());
    assert_eq!(leg.lines, vec![LEFT.to_string()]);
    let again = run_leg(&item, "a", Some("uuid-b"));
    assert!(again.probes.is_empty(), "one probe per token per window");
    assert!(again.restores.is_empty());
    assert!(again.lines.is_empty(), "{:?}", again.lines);
    assert_eq!(
        (
            fs::read(store("a")).expect("read"),
            fs::read(store("b")).expect("read")
        ),
        stores_before
    );
}

#[test]
fn the_leg_names_a_failed_lookup_and_waits_out_the_window() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    roster(&["a"], "a");
    let before = fs::read(store("a")).expect("read");
    let item = pair("lookup-acc", "lookup-ref", 2000);
    let leg = run_leg_with(&item, Some("a"), Err("connection refused"), true);
    assert_eq!(leg.probes, vec!["lookup-acc".to_string()]);
    assert!(leg.restores.is_empty());
    assert_eq!(
        leg.lines,
        line(
            "the macOS Keychain holds a login no clauth account has saved, and clauth could \
             not look up its owner (connection refused); left it in place, trying again in 15 \
             minutes"
        )
    );
    let again = run_leg_with(&item, Some("a"), Ok("uuid-a"), true);
    assert!(again.probes.is_empty(), "one probe per token per window");
    assert_eq!(fs::read(store("a")).expect("read"), before);
}

#[test]
fn the_leg_leaves_a_login_its_owners_store_cannot_take() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "b-ref", 1000), Some("uuid-b"));
    roster(&["a", "b"], "a");
    stage("b");
    let before = fs::read(store("b")).expect("read");
    let leg = run_leg(&pair("staged-acc", "staged-ref", 2000), "a", Some("uuid-b"));
    assert!(leg.restores.is_empty());
    assert_eq!(fs::read(store("b")).expect("read"), before);
    assert_eq!(
        leg.lines,
        line(
            "the macOS Keychain holds b's login, but b's saved login is mid-rotation or \
             unreadable; left it in place, trying again in 15 minutes"
        )
    );
}

#[test]
fn the_leg_places_nothing_two_profiles_could_both_claim() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    profile_holding("b", &pair("b-acc", "twice-ref", 1000), Some("uuid-shared"));
    profile_holding("c", &pair("c-acc", "twice-ref", 1000), Some("uuid-shared"));
    roster(&["a", "b", "c"], "a");

    // Two stores hold the token: it is saved, so the active login goes back
    // with nothing carried and no probe.
    let held_twice = run_leg(&pair("twice-acc", "twice-ref", 2000), "a", Some("uuid-b"));
    assert!(
        held_twice.probes.is_empty(),
        "a token two stores hold is not probed"
    );
    assert_eq!(held_twice.restores, vec![None]);
    assert!(held_twice.lines.is_empty(), "{:?}", held_twice.lines);

    let before = fs::read(store("b")).expect("read");
    let one_id = run_leg(
        &pair("shared-acc", "shared-ref", 2000),
        "a",
        Some("uuid-shared"),
    );
    assert_eq!(one_id.probes, vec!["shared-acc".to_string()]);
    assert!(one_id.restores.is_empty());
    assert_eq!(one_id.lines, vec![LEFT.to_string()]);
    assert_eq!(fs::read(store("b")).expect("read"), before);
}

/// An expired unsaved item cannot be looked up (`/profile` answers 401 for
/// good) and must not be overwritten: the leg skips the probe, writes
/// nothing, and says so once per window.
#[test]
fn the_leg_leaves_an_expired_unsaved_login_without_a_probe() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    roster(&["a"], "a");
    let item = expired_pair("lapsed-leg-acc", "lapsed-leg-ref");
    let first = run_leg(&item, "a", Some("uuid-a"));
    assert!(first.probes.is_empty(), "an expired token is never probed");
    assert!(first.restores.is_empty());
    assert_eq!(
        first.lines,
        line(
            "the macOS Keychain holds an expired login no clauth account has saved; left it in \
             place until Claude Code refreshes it"
        )
    );
    let again = run_leg(&item, "a", Some("uuid-a"));
    assert!(again.lines.is_empty(), "once per window: {:?}", again.lines);
    assert!(again.probes.is_empty() && again.restores.is_empty());
}

/// A spent item serves a dead login: the active login goes back over it.
#[test]
fn the_leg_puts_the_active_login_back_over_a_spent_item() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("a-acc", "a-ref", 1000), Some("uuid-a"));
    spend("a", "dead-ref");
    roster(&["a"], "a");
    let spent = run_leg(&pair("dead-acc", "dead-ref", 2000), "a", Some("uuid-a"));
    assert!(spent.probes.is_empty(), "a spent token needs no probe");
    assert_eq!(spent.restores, vec![None]);
    assert_eq!(
        spent.lines,
        line(
            "the macOS Keychain held a login that no longer works (its token was already \
             rotated); put a back"
        )
    );
}

/// The carry into an owner refuses an item whose chain another roster
/// profile's store already holds: that login is saved, and copying it would
/// put one single-use chain in two stores.
#[test]
fn a_carry_never_copies_a_chain_another_store_holds() {
    let _home = HomeSandbox::new();
    profile_holding("a", &pair("at-a", "rt-a", 1), None);
    profile_holding("b", &pair("at-x", "rt-x", 1), None);
    roster(&["a", "b"], "a");
    let err = carry("a", &pair("at-x-later", "rt-x", 5)).expect_err("b's chain refuses");
    assert_eq!(err.to_string(), "this login is already saved in 'b'");
    assert_eq!(read_json(&store("a")), pair("at-a", "rt-a", 1), "a's store");
    assert_eq!(read_json(&store("b")), pair("at-x", "rt-x", 1), "b's store");
}
