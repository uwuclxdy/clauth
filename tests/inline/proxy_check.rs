#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

const ADMIN: &str = "adm-0123456789abcdef0123456789abcdef";
const KEY: &str = "clp_stub-key-0";
const ACCOUNT: &str = "acct-1";

// ── the stub proxy ──────────────────────────────────────────────────────────
//
// A contract v1 proxy small enough to read whole: one account, one declared
// action, every figure kind, and a mutation log the tests read to prove what
// a run changed. Each `Plant` breaks exactly one thing the check must name.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    NoConfigRoute,
    UsedAsString,
    UsageWithoutToken,
    ContractTwo,
    ContractPlusOne,
    OtherService,
    MissingSettingValue,
    EchoSecrets,
    EscapeInSettingKey,
    CorsOnHealth,
    RemintKeepsOldKey,
    DeleteKeepsKey,
    CancelledFlowReadsFailed,
    IgnoreRebindAccount,
    LoginWithoutToken,
    LoginAnswers200,
    EchoFreshKey,
    TextConfig,
    BigHealth,
    SlowHealth,
    BearerAnyKey,
    StartPlanAccount,
    UppercaseAccountId,
    CorsOnInfo,
    SseNoHead,
    /// A `null` in a settings patch keeps the current value.
    NullResetKeeps,
    /// An account setting starts off its default (`off_peak: true`).
    OffPeakOn,
    /// Every login flow is poll-only; a paste into one answers 409.
    PollOnly,
    /// A poll-only flow still takes a paste.
    PollOnlyAcceptsPaste,
    /// A login start omits `modes`; the flow's doors are unknown.
    NoModes,
    /// A wrong method on a known control path answers 404, not 405.
    No405,
    /// A control body over 4096 bytes is served like any other.
    NoBodyCap,
    /// `GET /config` leaves out a declared proxy setting's value.
    ConfigMissingValue,
    /// The login answers 200, and its cancel leaves the flow readable.
    LoginAnswers200KeepsFlow,
    /// The re-bind probe opens a flow, and its cancel leaves it readable.
    StrayFlowKept,
    /// The re-bind probe opens a flow, and its cancel answers 404 while
    /// removing the flow.
    StrayFlowDelete404,
    /// `/health` answers bytes that are not HTTP.
    HealthNotHttp,
    /// `/health` closes the connection without a byte.
    HealthCloses,
    /// A proxy setting declares no `default`, and `GET /config` answers
    /// `null` for it.
    NoDefaultNull,
    /// A proxy setting declares an explicit `null` default.
    NullDefault,
    /// An account setting declares no `default`.
    NoDefaultReset,
    /// A login start answers an empty `modes` array.
    EmptyModes,
    /// A login start answers a `modes` array that is not poll-only.
    PasteMode,
    /// `/health` answers a body over the 1 MiB read cap.
    BigHealthBody,
    /// `off_peak`'s `active_when.key` is the literal text `a\u{001b}b`.
    LiteralEscapeInActiveKey,
    /// `off_peak`'s `active_when.key` holds a real ESC.
    EscapeInActiveKey,
    /// A contract 1.1 proxy: the credit figure declares its `model`.
    ContractOneOne,
    /// A contract 1.2 proxy, a minor past every one clauth knows.
    ContractOneTwo,
    /// A contract 1.1 proxy declares a figure `model` that is a number.
    ModelNotString,
    /// A contract 1.1 proxy declares an empty figure `model`.
    ModelEmpty,
    /// A contract 1.1 proxy declares a `null` figure `model`.
    ModelNull,
    /// A contract 1.3 proxy declares an empty figure `model`.
    ModelEmptyOnOneThree,
    /// A contract 1.3 proxy, a minor past every one clauth knows.
    ContractOneThree,
    /// A contract 1.2 proxy listing `token_stats`, its token route conformant.
    TokenStats,
    /// `TokenStats`, its token route answering without the admin token.
    TokensWithoutToken,
    /// `TokenStats`, its record's `output_tokens` the string `"1"`.
    TokenOutputString,
    /// `TokenStats`, its record's `model` a `null`.
    TokenModelNull,
    /// `TokenStats`, a read without `since` carrying `dropped: false`.
    TokensDroppedFalse,
    /// `TokenStats`, a read naming a cursor it never served answering
    /// without `dropped`.
    UnknownCursorNotDropped,
    /// A contract 1.2 proxy without `token_stats` whose token route answers
    /// 404 rather than 501.
    Tokens404,
    /// A contract 1.0 proxy declares a figure `model` that is a number, a
    /// field 1.0 does not define.
    ModelNumberOnOneZero,
    /// `TokenStats`, its ring's first record `id` 129 astral characters (258
    /// UTF-16 units), its second 128 (256 units).
    TokenIdPastUtf16Max,
}

impl Plant {
    /// The contract `/health` and `/info` both declare.
    fn contract(self) -> &'static str {
        match self {
            Self::ContractTwo => "2.0",
            Self::ContractPlusOne => "+1.0",
            Self::ContractOneOne | Self::ModelNotString | Self::ModelEmpty | Self::ModelNull => {
                "1.1"
            }
            Self::ContractOneTwo | Self::Tokens404 => "1.2",
            _ if self.token_stats() => "1.2",
            Self::ContractOneThree | Self::ModelEmptyOnOneThree => "1.3",
            _ => "1.0",
        }
    }

    /// Whether `/info` lists the `token_stats` capability.
    fn token_stats(self) -> bool {
        matches!(
            self,
            Self::TokenStats
                | Self::TokensWithoutToken
                | Self::TokenOutputString
                | Self::TokenModelNull
                | Self::TokensDroppedFalse
                | Self::UnknownCursorNotDropped
                | Self::TokenIdPastUtf16Max
        )
    }
}

/// The one cursor the stub's token route serves.
const CURSOR: &str = "stub-life.1";

struct StubState {
    plant: Plant,
    accounts: Vec<Value>,
    keys: BTreeMap<String, String>,
    flows: BTreeMap<String, Value>,
    next_flow: u32,
    next_key: u32,
    config: serde_json::Map<String, Value>,
    mutations: Vec<String>,
    inferences: u32,
    /// Every request's method and path, in arrival order.
    requests: Vec<String>,
    /// Every flow a re-bind request opened.
    rebinds: Vec<String>,
    /// Every request's target, query included, in arrival order.
    targets: Vec<String>,
    /// The targets of the requests that carried the admin token.
    authed_targets: Vec<String>,
}

impl StubState {
    fn new(plant: Plant) -> Self {
        let settings = if plant == Plant::MissingSettingValue {
            json!({"plan": "coding-plan", "timezone": null})
        } else if plant == Plant::StartPlanAccount {
            json!({"plan": "start-plan", "off_peak": false, "timezone": null})
        } else if plant == Plant::OffPeakOn {
            json!({"plan": "coding-plan", "off_peak": true, "timezone": null})
        } else if plant == Plant::NoDefaultReset {
            json!({"plan": "coding-plan", "off_peak": false, "timezone": null, "k": false})
        } else {
            json!({"plan": "coding-plan", "off_peak": false, "timezone": null})
        };
        let mut config = serde_json::Map::new();
        config.insert("host_identity".into(), json!(false));
        if matches!(plant, Plant::NoDefaultNull | Plant::NullDefault) {
            config.insert("k".into(), Value::Null);
        }
        let account = if plant == Plant::UppercaseAccountId {
            "Acct-1"
        } else {
            ACCOUNT
        };
        Self {
            plant,
            accounts: vec![json!({
                "id": account,
                "label": "stub@example.com",
                "state": "ready",
                "created_at": "2026-09-29T10:00:00Z",
                "plans": [{"id": "coding-plan", "label": "coding plan"}],
                "settings": settings,
            })],
            keys: BTreeMap::from([(KEY.to_string(), account.to_string())]),
            flows: BTreeMap::new(),
            next_flow: 1,
            next_key: 1,
            config,
            mutations: Vec::new(),
            inferences: 0,
            requests: Vec::new(),
            rebinds: Vec::new(),
            targets: Vec::new(),
            authed_targets: Vec::new(),
        }
    }
}

/// The value a fresh account starts with, per `info()`'s declarations.
fn setting_default(key: &str) -> Value {
    match key {
        "plan" => json!("coding-plan"),
        "off_peak" => json!(false),
        _ => Value::Null,
    }
}

/// Whether `segs` names a control path some method serves, so another
/// method on it answers 405 `method_not_allowed`.
fn known_path(segs: &[&str]) -> bool {
    matches!(
        segs,
        ["info" | "accounts" | "usage" | "config" | "events" | "tokens"]
            | ["accounts", _]
            | ["accounts", "login", _]
            | ["accounts", _, "key"]
            | ["accounts", _, "actions", _]
    )
}

fn info() -> Value {
    json!({
        "service": "stub",
        "version": "0.0.1",
        "capabilities": ["events"],
        "actions": [{"name": "claim-now", "label": "claim now", "scope": "account", "target": "offer", "confirm": false}],
        "figures": [
            {"id": "plan-5h", "kind": "window", "label": "plan 5h", "chain": "5h"},
            {"id": "credit", "kind": "balance", "label": "credit"},
            {"id": "off-peak", "kind": "channel", "label": "off-peak"},
            {"id": "trials", "kind": "offer", "label": "trials"},
            {"id": "traffic", "kind": "stats", "label": "traffic"},
        ],
        "settings": [
            {"key": "plan", "scope": "account", "label": "plan", "hint": "which plan serves requests",
             "type": "enum", "default": "coding-plan",
             "options": [{"value": "coding-plan", "label": "coding plan"}, {"value": "start-plan", "label": "start plan"}]},
            {"key": "off_peak", "scope": "account", "label": "off-peak", "hint": "use the off-peak channel",
             "type": "bool", "default": false, "active_when": {"key": "plan", "in": ["coding-plan"]},
             "inactive_hint": "coding plan only"},
            {"key": "timezone", "scope": "account", "label": "timezone", "hint": "IANA zone the account presents",
             "type": "string", "default": null},
            {"key": "host_identity", "scope": "proxy", "label": "host OS values", "hint": "send the host's own OS values",
             "type": "bool", "default": false, "restart": true},
        ],
    })
}

/// The key a plant names in `off_peak`'s `active_when`: the literal escape
/// text, or the control character it spells.
fn active_key(plant: Plant) -> &'static str {
    if plant == Plant::LiteralEscapeInActiveKey {
        "a\\u{001b}b"
    } else {
        "a\u{1b}b"
    }
}

fn usage_of(account: &str, plant: Plant) -> Value {
    let used = if plant == Plant::UsedAsString {
        json!("1234567")
    } else {
        json!(1_234_567)
    };
    json!({
        "account": account,
        "state": "ready",
        "available": true,
        "figures": [
            {"kind": "window", "id": "plan-5h", "label": "plan 5h", "used": used, "limit": 4_000_000,
             "unit": "tokens", "window_secs": 18000, "resets_at": "2026-09-29T15:00:00Z", "chain": "5h",
             "read_at": "2026-09-29T12:01:00Z"},
            {"kind": "balance", "id": "credit", "label": "credit", "remaining": 820_000, "limit": 1_000_000,
             "used": 180_000, "unit": "tokens", "expires_at": "2026-10-05T00:00:00Z", "read_at": "2026-09-29T12:00:40Z"},
            {"kind": "channel", "id": "off-peak", "label": "off-peak", "open": false,
             "next_open_at": "2026-09-29T16:00:00Z", "read_at": "2026-09-29T12:01:00Z"},
            {"kind": "offer", "id": "trials", "label": "trials", "read_at": "2026-09-29T12:00:00Z",
             "items": [{"id": "weekend", "label": "weekend plan", "state": "available",
                        "grants": [{"label": "tokens", "amount": 5_000_000, "unit": "tokens"}]}]},
            {"kind": "stats", "id": "traffic", "label": "traffic", "since": "2026-09-29T08:00:00Z",
             "requests": {"attempted": 3, "succeeded": 3, "failed": 0, "cancelled": 0}, "mean_latency_ms": 840,
             "tokens": {"input": 210, "output": 48, "cache_read": 900, "cache_creation": 30},
             "read_at": "2026-09-29T12:01:05Z"},
        ],
    })
}

struct Reply {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: String,
    /// Bytes written as the whole answer instead, then the socket closes.
    raw: Option<String>,
}

fn ok(status: u16, body: Value) -> Reply {
    Reply {
        status,
        content_type: "application/json",
        headers: Vec::new(),
        body: body.to_string(),
        raw: None,
    }
}

fn control_err(status: u16, code: &str, field: Option<&str>) -> Reply {
    let mut body = json!({"ok": false, "error": code, "reason": "stub refusal"});
    if let Some(field) = field {
        body["field"] = json!(field);
    }
    ok(status, body)
}

fn anthropic_err(status: u16, kind: &str) -> Reply {
    ok(
        status,
        json!({"type": "error", "error": {"type": kind, "message": "stub refusal"}}),
    )
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn setting_fits(key: &str, value: &Value) -> Option<bool> {
    match key {
        "plan" => Some(matches!(value.as_str(), Some("coding-plan" | "start-plan"))),
        "off_peak" => Some(value.is_boolean()),
        "timezone" => Some(value.is_string() || value.is_null()),
        "k" => Some(value.is_boolean()),
        _ => None,
    }
}

fn handle(
    s: &mut StubState,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: &str,
) -> Reply {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if method == "GET" && path == "/health" {
        match s.plant {
            Plant::HealthNotHttp => {
                return Reply {
                    raw: Some("SSH-2.0-not-http\r\n\r\n".into()),
                    ..ok(200, Value::Null)
                };
            }
            Plant::HealthCloses => {
                return Reply {
                    raw: Some(String::new()),
                    ..ok(200, Value::Null)
                };
            }
            _ => {}
        }
        let contract = s.plant.contract();
        let status = if s.plant == Plant::EchoSecrets {
            ADMIN
        } else {
            "ok"
        };
        let service = if s.plant == Plant::OtherService {
            "other-proxy"
        } else {
            "stub"
        };
        let mut health =
            json!({"status": status, "service": service, "version": "0.0.1", "contract": contract});
        if s.plant == Plant::BigHealth {
            // Padded to exactly the 4096-byte cap, which the draft puts out of bounds.
            health["pad"] = json!("");
            let short = health.to_string().len();
            health["pad"] = json!("x".repeat(4096 - short));
        }
        if s.plant == Plant::BigHealthBody {
            // Padded past the 1 MiB read cap, which the check reads as a
            // bounded read failure, never a timed-out answer.
            health["pad"] = json!("");
            let short = health.to_string().len();
            health["pad"] = json!("x".repeat(BODY_LIMIT as usize + 1 - short));
        }
        if s.plant == Plant::SlowHealth {
            std::thread::sleep(
                Duration::from_secs(crate::gateway::HEALTH_RESPONSE_SECS)
                    + Duration::from_millis(500),
            );
        }
        let mut reply = ok(200, health);
        if s.plant == Plant::CorsOnHealth {
            reply
                .headers
                .push(("Access-Control-Allow-Origin".into(), "*".into()));
        }
        return reply;
    }
    if let Some(rest) = path.strip_prefix("/clauth/v1/") {
        let segs: Vec<&str> = rest.split('/').collect();
        let exempt = (s.plant == Plant::UsageWithoutToken && method == "GET" && segs == ["usage"])
            || (s.plant == Plant::TokensWithoutToken && method == "GET" && segs == ["tokens"])
            || (s.plant == Plant::LoginWithoutToken
                && method == "POST"
                && segs == ["accounts", "login"]);
        let bearer = header(headers, "authorization").and_then(|v| v.strip_prefix("Bearer "));
        if bearer != Some(ADMIN) && !exempt {
            let mut reply = control_err(401, "unauthorized", None);
            reply
                .headers
                .push(("WWW-Authenticate".into(), "Bearer".into()));
            if s.plant == Plant::CorsOnInfo && segs == ["info"] {
                reply
                    .headers
                    .push(("Access-Control-Allow-Credentials".into(), "true".into()));
            }
            return reply;
        }
        // The core reads every control body through one 4096-byte cap.
        if body.len() > 4096 && s.plant != Plant::NoBodyCap {
            return control_err(413, "payload_too_large", None);
        }
        let mut reply = control(s, method, &segs, query, body);
        if s.plant == Plant::CorsOnInfo && segs == ["info"] {
            reply
                .headers
                .push(("Access-Control-Allow-Credentials".into(), "true".into()));
        }
        return reply;
    }
    if method == "POST" && path == "/v1/messages" {
        let api = header(headers, "x-api-key");
        let bearer = header(headers, "authorization").and_then(|v| v.strip_prefix("Bearer "));
        let key = match (api, bearer) {
            (Some(a), Some(b)) if a != b => return anthropic_err(401, "authentication_error"),
            (Some(k), _) | (None, Some(k)) => k,
            (None, None) => return anthropic_err(401, "authentication_error"),
        };
        let any_bearer = s.plant == Plant::BearerAnyKey && api.is_none();
        if !s.keys.contains_key(key) && !any_bearer {
            return anthropic_err(401, "authentication_error");
        }
        if serde_json::from_str::<Value>(body).is_err() {
            return anthropic_err(400, "invalid_request_error");
        }
        s.inferences += 1;
        // Echoes a re-minted key back, the one a later run must never print.
        let kind = if s.plant == Plant::EchoFreshKey && key != KEY {
            key
        } else {
            "message"
        };
        return ok(
            200,
            json!({"id": "msg_1", "type": kind, "role": "assistant", "model": "stub-model",
                   "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
                   "usage": {"input_tokens": 5, "output_tokens": 1}}),
        );
    }
    anthropic_err(404, "not_found_error")
}

fn control(s: &mut StubState, method: &str, segs: &[&str], query: &str, body: &str) -> Reply {
    let body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let account_index = |s: &StubState, id: &str| s.accounts.iter().position(|a| a["id"] == id);
    match (method, segs) {
        ("GET", ["info"]) => {
            let mut info = info();
            if s.plant == Plant::EchoSecrets {
                info["version"] = json!(KEY);
            }
            info["contract"] = json!(s.plant.contract());
            let model = match s.plant {
                Plant::ContractOneOne | Plant::ContractOneTwo => Some(json!("glm-5")),
                Plant::ModelNotString | Plant::ModelNumberOnOneZero => Some(json!(5)),
                Plant::ModelEmpty | Plant::ModelEmptyOnOneThree => Some(json!("")),
                Plant::ModelNull => Some(Value::Null),
                _ => None,
            };
            if let Some(model) = model {
                info["figures"][1]["model"] = model;
            }
            if s.plant.token_stats() {
                info["capabilities"] = json!(["events", "token_stats"]);
            }
            let settings = info["settings"].as_array_mut().unwrap();
            if matches!(
                s.plant,
                Plant::LiteralEscapeInActiveKey | Plant::EscapeInActiveKey
            ) {
                let key = json!(active_key(s.plant));
                if let Some(off_peak) = settings.iter_mut().find(|s| s["key"] == "off_peak") {
                    off_peak["active_when"]["key"] = key;
                }
            }
            match s.plant {
                Plant::NoDefaultNull => settings.push(json!({
                    "key": "k", "scope": "proxy", "label": "k", "hint": "k", "type": "bool"
                })),
                Plant::NullDefault => settings.push(json!({
                    "key": "k", "scope": "proxy", "label": "k", "hint": "k",
                    "type": "bool", "default": null
                })),
                Plant::NoDefaultReset => settings.push(json!({
                    "key": "k", "scope": "account", "label": "k", "hint": "k", "type": "bool"
                })),
                _ => {}
            }
            ok(200, info)
        }
        ("GET", ["accounts"]) => {
            let mut accounts = json!(s.accounts);
            if s.plant == Plant::EscapeInSettingKey {
                accounts[0]["settings"]["\u{1b}[2J"] = json!(true);
            }
            ok(200, json!({"accounts": accounts}))
        }
        ("POST", ["accounts", "login"]) => {
            let rebind = body["account"].as_str().map(str::to_string);
            if let Some(id) = &rebind
                && account_index(s, id).is_none()
                && !matches!(
                    s.plant,
                    Plant::IgnoreRebindAccount | Plant::StrayFlowKept | Plant::StrayFlowDelete404
                )
            {
                return control_err(404, "not_found", None);
            }
            let flow = format!("f_{}", s.next_flow);
            s.next_flow += 1;
            let view = if s.plant == Plant::NoModes {
                json!({"flow": flow, "state": "pending", "url": format!("https://stub.example/login/{flow}"),
                       "poll_interval_ms": 1000, "expires_at": "2026-09-29T12:10:00Z"})
            } else {
                let modes = if matches!(s.plant, Plant::PollOnly | Plant::PollOnlyAcceptsPaste) {
                    json!(["poll"])
                } else if s.plant == Plant::EmptyModes {
                    json!([])
                } else if s.plant == Plant::PasteMode {
                    json!(["Paste"])
                } else {
                    json!(["poll", "paste"])
                };
                json!({"flow": flow, "state": "pending", "url": format!("https://stub.example/login/{flow}"),
                       "modes": modes, "poll_interval_ms": 1000,
                       "expires_at": "2026-09-29T12:10:00Z"})
            };
            if rebind.is_some() {
                s.rebinds.push(flow.clone());
            }
            s.flows.insert(flow, view.clone());
            s.mutations.push(match rebind {
                Some(id) => format!("POST /accounts/login account={id}"),
                None => "POST /accounts/login".into(),
            });
            ok(
                if matches!(
                    s.plant,
                    Plant::LoginAnswers200 | Plant::LoginAnswers200KeepsFlow
                ) {
                    200
                } else {
                    201
                },
                view,
            )
        }
        ("GET", ["accounts", "login", flow]) => match s.flows.get(*flow) {
            Some(view) => ok(200, view.clone()),
            None => control_err(404, "not_found", None),
        },
        ("POST", ["accounts", "login", flow]) => match s.flows.get(*flow) {
            Some(view)
                if !view["modes"]
                    .as_array()
                    .is_some_and(|m| m.contains(&json!("paste")))
                    && !matches!(
                        s.plant,
                        Plant::PollOnlyAcceptsPaste | Plant::EmptyModes | Plant::PasteMode
                    ) =>
            {
                control_err(409, "flow_not_paste", None)
            }
            Some(view) => ok(200, view.clone()),
            None => control_err(404, "not_found", None),
        },
        ("DELETE", ["accounts", "login", flow]) => match s.flows.remove(*flow) {
            Some(mut view) => {
                s.mutations.push(format!("DELETE /accounts/login/{flow}"));
                let stray = s.rebinds.iter().any(|f| f == flow);
                if matches!(
                    s.plant,
                    Plant::CancelledFlowReadsFailed | Plant::LoginAnswers200KeepsFlow
                ) || (s.plant == Plant::StrayFlowKept && stray)
                {
                    view["state"] = json!("failed");
                    s.flows.insert((*flow).to_string(), view);
                }
                if s.plant == Plant::StrayFlowDelete404 && stray {
                    control_err(404, "not_found", None)
                } else {
                    ok(204, Value::Null)
                }
            }
            None => control_err(404, "not_found", None),
        },
        ("PATCH", ["accounts", id]) => {
            let Some(i) = account_index(s, id) else {
                return control_err(404, "not_found", None);
            };
            let Some(settings) = body["settings"].as_object() else {
                return control_err(400, "bad_request", None);
            };
            for (k, v) in settings {
                match setting_fits(k, v) {
                    None => return control_err(422, "unknown_setting", Some(k)),
                    // `null` resets a key to its default.
                    Some(false) if v.is_null() => {}
                    Some(false) => return control_err(422, "invalid_setting", Some(k)),
                    Some(true) => {}
                }
            }
            for (k, v) in settings {
                if v.is_null() {
                    if s.plant != Plant::NullResetKeeps {
                        s.accounts[i]["settings"][k] = setting_default(k);
                    }
                    s.mutations.push(format!("PATCH /accounts/{id} {k}=null"));
                    continue;
                }
                s.accounts[i]["settings"][k] = v.clone();
                let shown = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                s.mutations
                    .push(format!("PATCH /accounts/{id} {k}={shown}"));
            }
            ok(200, s.accounts[i].clone())
        }
        ("POST", ["accounts", id, "key"]) => {
            if account_index(s, id).is_none() {
                return control_err(404, "not_found", None);
            }
            let fresh = format!("clp_stub-key-{}", s.next_key);
            s.next_key += 1;
            if s.plant != Plant::RemintKeepsOldKey {
                s.keys.retain(|_, owner| owner != id);
            }
            s.keys.insert(fresh.clone(), (*id).to_string());
            s.mutations.push(format!("POST /accounts/{id}/key"));
            ok(200, json!({"inference_key": fresh}))
        }
        ("DELETE", ["accounts", id]) => {
            let Some(i) = account_index(s, id) else {
                return control_err(404, "not_found", None);
            };
            s.accounts.remove(i);
            if s.plant != Plant::DeleteKeepsKey {
                s.keys.retain(|_, owner| owner != id);
            }
            s.mutations.push(format!("DELETE /accounts/{id}"));
            ok(204, Value::Null)
        }
        ("POST", ["accounts", id, "actions", name]) => {
            if account_index(s, id).is_none() || *name != "claim-now" {
                return control_err(404, "not_found", None);
            }
            s.mutations
                .push(format!("POST /accounts/{id}/actions/{name}"));
            ok(202, json!({"accepted": true}))
        }
        ("GET", ["usage"]) => {
            let only = query.strip_prefix("account=");
            let accounts: Vec<Value> = s
                .accounts
                .iter()
                .filter_map(|a| a["id"].as_str())
                .filter(|id| only.is_none_or(|o| o == *id))
                .map(|id| usage_of(id, s.plant))
                .collect();
            ok(200, json!({"accounts": accounts}))
        }
        ("GET", ["config"]) if s.plant == Plant::NoConfigRoute => {
            control_err(404, "not_found", None)
        }
        ("GET", ["config"]) => {
            let values = if s.plant == Plant::ConfigMissingValue {
                json!({})
            } else {
                json!(s.config)
            };
            let mut reply = ok(200, json!({"values": values}));
            if s.plant == Plant::TextConfig {
                reply.content_type = "text/plain";
            }
            reply
        }
        ("PATCH", ["config"]) => {
            let Some(values) = body["values"].as_object() else {
                return control_err(400, "bad_request", None);
            };
            for (k, v) in values {
                if k != "host_identity" {
                    return control_err(422, "unknown_setting", Some(k));
                }
                if !v.is_boolean() {
                    return control_err(422, "invalid_setting", Some(k));
                }
            }
            for (k, v) in values {
                s.config.insert(k.clone(), v.clone());
                s.mutations.push(format!("PATCH /config {k}={v}"));
            }
            ok(200, json!({"values": s.config}))
        }
        ("GET", ["tokens"]) if s.plant == Plant::TokenIdPastUtf16Max => {
            let widest = json!([
                {
                    "id": "\u{1F600}".repeat(129), "account": ACCOUNT, "model": "stub-model",
                    "output_tokens": 1, "started_at": "2026-09-29T10:00:00Z",
                    "ended_at": "2026-09-29T10:00:01Z", "ttft_ms": 120,
                },
                {
                    "id": "\u{1F600}".repeat(128), "account": ACCOUNT, "model": "stub-model",
                    "output_tokens": 1, "started_at": "2026-09-29T10:00:00Z",
                    "ended_at": "2026-09-29T10:00:01Z", "ttft_ms": 120,
                },
            ]);
            let since = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="));
            let body = match since {
                None => json!({"records": widest, "cursor": CURSOR}),
                Some(CURSOR) => json!({"records": [], "cursor": CURSOR}),
                Some(_) => json!({"records": widest, "cursor": CURSOR, "dropped": true}),
            };
            ok(200, body)
        }
        ("GET", ["tokens"]) if s.plant.token_stats() => {
            let mut record = json!({
                "id": "msg_1", "account": ACCOUNT, "model": "stub-model", "output_tokens": 1,
                "started_at": "2026-09-29T10:00:00Z", "ended_at": "2026-09-29T10:00:01Z",
                "ttft_ms": 120,
            });
            match s.plant {
                Plant::TokenOutputString => record["output_tokens"] = json!("1"),
                Plant::TokenModelNull => record["model"] = Value::Null,
                _ => {}
            }
            let since = query
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="));
            let body = match since {
                None if s.plant == Plant::TokensDroppedFalse => {
                    json!({"records": [record], "cursor": CURSOR, "dropped": false})
                }
                None => json!({"records": [record], "cursor": CURSOR}),
                Some(CURSOR) => json!({"records": [], "cursor": CURSOR}),
                Some(_) if s.plant == Plant::UnknownCursorNotDropped => {
                    json!({"records": [record], "cursor": CURSOR})
                }
                Some(_) => json!({"records": [record], "cursor": CURSOR, "dropped": true}),
            };
            ok(200, body)
        }
        ("GET", ["tokens"]) if s.plant == Plant::Tokens404 => control_err(404, "not_found", None),
        ("GET", ["tokens"]) => control_err(501, "unsupported", None),
        ("GET", ["events"]) if s.plant == Plant::SseNoHead => Reply {
            raw: Some("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n".into()),
            ..ok(200, Value::Null)
        },
        ("GET", ["events"]) => Reply {
            status: 200,
            content_type: "text/event-stream",
            headers: Vec::new(),
            body: ": keepalive\n\n".into(),
            raw: None,
        },
        _ if known_path(segs) && s.plant != Plant::No405 => {
            control_err(405, "method_not_allowed", None)
        }
        _ => control_err(404, "not_found", None),
    }
}

struct Stub {
    base: String,
    state: Arc<Mutex<StubState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Stub {
    fn start(plant: Plant) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let base = format!("http://{}", listener.local_addr().expect("local_addr"));
        let state = Arc::new(Mutex::new(StubState::new(plant)));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, stopping) = (Arc::clone(&state), Arc::clone(&stop));
        let thread = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => serve_one(sock, &shared),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            base,
            state,
            stop,
            thread: Some(thread),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, StubState> {
        self.state.lock().expect("stub state")
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

fn serve_one(mut sock: TcpStream, state: &Mutex<StubState>) {
    sock.set_nonblocking(false).ok();
    sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut req = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        match sock.read(&mut tmp) {
            Ok(0) | Err(_) => return,
            Ok(n) => req.extend_from_slice(&tmp[..n]),
        }
        if let Some(h) = req.windows(4).position(|w| w == b"\r\n\r\n") {
            break h;
        }
    };
    let head = String::from_utf8_lossy(&req[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (
        first.next().unwrap_or("").to_string(),
        first.next().unwrap_or("").to_string(),
    );
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let len = header(&headers, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    while req.len() < head_end + 4 + len {
        match sock.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => req.extend_from_slice(&tmp[..n]),
        }
    }
    let body = String::from_utf8_lossy(&req[head_end + 4..]).into_owned();
    let mut state = state.lock().expect("stub state");
    let path = target
        .split_once('?')
        .map_or(target.as_str(), |(path, _)| path);
    state.requests.push(format!("{method} {path}"));
    state.targets.push(target.clone());
    if header(&headers, "authorization") == Some(format!("Bearer {ADMIN}").as_str()) {
        state.authed_targets.push(target.clone());
    }
    let reply = handle(&mut state, &method, &target, &headers, &body);
    if let Some(raw) = reply.raw {
        let _ = sock.write_all(raw.as_bytes());
        let _ = sock.shutdown(std::net::Shutdown::Both);
        return;
    }
    let body = if reply.status == 204 {
        String::new()
    } else {
        reply.body
    };
    let extra: String = reply
        .headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let head = format!(
        "HTTP/1.1 {} X\r\nContent-Type: {}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        body.len()
    );
    let _ = sock.write_all(head.as_bytes());
    let _ = sock.write_all(body.as_bytes());
    let _ = sock.shutdown(std::net::Shutdown::Write);
}

fn secrets() -> (Secret, Secret) {
    (Secret::new(ADMIN.to_string()), Secret::new(KEY.to_string()))
}

fn violation(route: &str, expected: &str, got: &str) -> Violation {
    Violation {
        route: route.to_string(),
        expected: expected.to_string(),
        got: got.to_string(),
    }
}

// ── the check against the stub ──────────────────────────────────────────────

#[test]
fn a_safe_run_on_a_conformant_proxy_names_no_violation_and_changes_nothing() {
    let stub = Stub::start(Plant::None);
    let (admin, key) = secrets();
    let before = {
        let state = stub.state();
        (
            state.accounts.clone(),
            state.keys.clone(),
            state.config.clone(),
        )
    };

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(report.skipped, Vec::<String>::new());
    let state = stub.state();
    assert_eq!(
        state.mutations,
        vec![
            "POST /accounts/login".to_string(),
            "DELETE /accounts/login/f_1".to_string()
        ]
    );
    assert_eq!(
        (
            state.accounts.clone(),
            state.keys.clone(),
            state.config.clone()
        ),
        before
    );
    assert_eq!(state.inferences, 1);
}

#[test]
fn a_destructive_run_consumes_the_named_account_and_names_no_violation() {
    let stub = Stub::start(Plant::None);
    let (admin, key) = secrets();

    let report = check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: ACCOUNT.to_string(),
        },
    )
    .expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    // A free string setting has no other value the check can pick safely.
    assert_eq!(
        report.skipped,
        vec!["setting timezone: no other valid value to set".to_string()]
    );
    let state = stub.state();
    assert_eq!(
        state.mutations,
        [
            "POST /accounts/login",
            "DELETE /accounts/login/f_1",
            "POST /accounts/login account=acct-1",
            "DELETE /accounts/login/f_2",
            "PATCH /accounts/acct-1 plan=start-plan",
            "PATCH /accounts/acct-1 plan=null",
            "PATCH /accounts/acct-1 plan=coding-plan",
            "PATCH /accounts/acct-1 off_peak=true",
            "PATCH /accounts/acct-1 off_peak=null",
            "PATCH /accounts/acct-1 off_peak=false",
            "POST /accounts/acct-1/actions/claim-now",
            "POST /accounts/acct-1/key",
            "DELETE /accounts/acct-1",
        ]
        .map(String::from)
        .to_vec()
    );
    assert!(state.accounts.is_empty(), "the named account is consumed");
    assert!(state.keys.is_empty(), "no key survives the delete");
    // The safe leg's one request and the re-minted key's one.
    assert_eq!(state.inferences, 2);
}

#[test]
fn a_destructive_run_on_an_account_the_proxy_lacks_is_a_usage_error_naming_it() {
    let stub = Stub::start(Plant::None);
    let (admin, key) = secrets();

    let err = check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: "acct-9".to_string(),
        },
    )
    .expect_err("an unknown account is refused");

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        "--destructive names account \"acct-9\", which this proxy does not list (it lists: \"acct-1\")"
    );
    assert_eq!(
        stub.state().mutations,
        Vec::<String>::new(),
        "the refusal comes before any request that changes state"
    );
    assert_eq!(stub.state().inferences, 0, "and before the paid request");
}

#[test]
fn a_missing_route_fails_naming_it() {
    let stub = Stub::start(Plant::NoConfigRoute);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/config",
            "status 200",
            "status 404"
        )]
    );
}

#[test]
fn a_wrong_field_type_fails_naming_the_route_and_the_field() {
    let stub = Stub::start(Plant::UsedAsString);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![
            violation(
                "GET /clauth/v1/usage",
                "accounts[0].figures[0].used: number",
                "string"
            ),
            violation(
                "GET /clauth/v1/usage?account={id}",
                "accounts[0].figures[0].used: number",
                "string"
            ),
        ]
    );
}

#[test]
fn a_control_route_answering_without_the_token_fails_naming_it() {
    let stub = Stub::start(Plant::UsageWithoutToken);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/usage (no token)",
            "status 401",
            "status 200"
        )]
    );
}

#[test]
fn an_account_missing_a_declared_settings_value_fails_naming_it() {
    let stub = Stub::start(Plant::MissingSettingValue);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/accounts",
            "accounts[0].settings.off_peak: the setting's current value",
            "nothing"
        )]
    );
}

#[test]
fn a_foreign_contract_major_stops_the_check_at_health() {
    let stub = Stub::start(Plant::ContractTwo);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation("GET /health", "contract: major 1", "\"2.0\"")]
    );
    assert_eq!(stub.state().mutations, Vec::<String>::new());
    assert_eq!(stub.state().inferences, 0);
}

/// A major spelled with a sign is no `MAJOR.MINOR`: the check reads it
/// through the parser `clauth proxy enable` refuses it with, and stops there.
#[test]
fn a_signed_contract_major_stops_the_check_at_health() {
    let stub = Stub::start(Plant::ContractPlusOne);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation("GET /health", "contract: major 1", "\"+1.0\"")]
    );
    assert_eq!(stub.state().mutations, Vec::<String>::new());
    assert_eq!(stub.state().inferences, 0);
}

#[test]
fn a_contract_1_1_proxy_declaring_a_figure_model_is_checked_whole() {
    let stub = Stub::start(Plant::ContractOneOne);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(report.skipped, Vec::<String>::new());
    assert_eq!(stub.state().inferences, 1);
}

#[test]
fn a_minor_past_1_2_is_skipped_naming_the_minor_this_clauth_knows() {
    let stub = Stub::start(Plant::ContractOneThree);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(
        report.skipped,
        vec!["fields contract 1.3 adds beyond 1.2: this clauth knows 1.2".to_string()]
    );
}

/// The token route's reads, query included, in the order the check sent them.
fn token_reads(stub: &Stub) -> Vec<String> {
    stub.state()
        .targets
        .iter()
        .filter(|t| t.split('?').next() == Some("/clauth/v1/tokens"))
        .cloned()
        .collect()
}

#[test]
fn a_contract_1_2_proxy_serving_token_stats_is_checked_whole() {
    let stub = Stub::start(Plant::TokenStats);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(report.skipped, Vec::<String>::new());
    let reads = token_reads(&stub);
    assert!(
        reads.iter().any(|t| t == "/clauth/v1/tokens"),
        "a read without a cursor: {reads:?}"
    );
    assert!(
        reads
            .iter()
            .any(|t| *t == format!("/clauth/v1/tokens?since={CURSOR}")),
        "a read from the cursor the first read served: {reads:?}"
    );
    assert!(
        reads
            .iter()
            .any(|t| t.starts_with("/clauth/v1/tokens?since=")
                && *t != format!("/clauth/v1/tokens?since={CURSOR}")),
        "a read from a cursor the proxy never served: {reads:?}"
    );
    // The safe run's login leg opens one flow and cancels it, as on a 1.0
    // proxy; the token leg adds no mutation.
    assert_eq!(
        stub.state().mutations,
        vec![
            "POST /accounts/login".to_string(),
            "DELETE /accounts/login/f_1".to_string()
        ]
    );
}

#[test]
fn a_contract_1_2_proxy_without_token_stats_answers_its_token_route_501() {
    let stub = Stub::start(Plant::ContractOneTwo);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(report.skipped, Vec::<String>::new());
    let authed: Vec<String> = stub
        .state()
        .authed_targets
        .iter()
        .filter(|t| t.split('?').next() == Some("/clauth/v1/tokens"))
        .cloned()
        .collect();
    assert_eq!(
        authed,
        vec!["/clauth/v1/tokens".to_string()],
        "one read carrying the admin token, answered 501"
    );
}

#[test]
fn a_token_route_answering_404_without_the_capability_fails_naming_501() {
    let stub = Stub::start(Plant::Tokens404);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/tokens",
            "status 501",
            "status 404"
        )]
    );
}

#[test]
fn a_token_route_answering_without_the_token_fails_naming_it() {
    let stub = Stub::start(Plant::TokensWithoutToken);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/tokens (no token)",
            "status 401",
            "status 200"
        )]
    );
}

#[test]
fn a_token_record_output_that_is_no_integer_fails_naming_the_field() {
    let stub = Stub::start(Plant::TokenOutputString);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![
            violation(
                "GET /clauth/v1/tokens",
                "records[0].output_tokens: integer",
                "string"
            ),
            violation(
                "GET /clauth/v1/tokens?since={unknown}",
                "records[0].output_tokens: integer",
                "string"
            ),
        ]
    );
}

/// The contract's optional record fields are absent or set, never `null`.
#[test]
fn a_token_record_model_that_is_null_fails_as_no_string() {
    let stub = Stub::start(Plant::TokenModelNull);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![
            violation("GET /clauth/v1/tokens", "records[0].model: string", "null"),
            violation(
                "GET /clauth/v1/tokens?since={unknown}",
                "records[0].model: string",
                "null"
            ),
        ]
    );
}

#[test]
fn a_read_without_a_cursor_carrying_dropped_false_fails_naming_it() {
    let stub = Stub::start(Plant::TokensDroppedFalse);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/tokens",
            "dropped: absent",
            "false"
        )]
    );
}

#[test]
fn a_read_from_an_unknown_cursor_not_marked_dropped_fails_naming_it() {
    let stub = Stub::start(Plant::UnknownCursorNotDropped);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/tokens?since={unknown}",
            "dropped: true",
            "nothing"
        )]
    );
}

/// The token route is a 1.2 addition: an older proxy is never asked for it.
#[test]
fn a_contract_1_1_proxy_is_never_asked_for_its_token_route() {
    let stub = Stub::start(Plant::ContractOneOne);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(token_reads(&stub), Vec::<String>::new());
}

#[test]
fn a_figure_model_that_is_no_string_fails_naming_the_field() {
    let stub = Stub::start(Plant::ModelNotString);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/info",
            "figures[1].model: string",
            "number"
        )]
    );
}

#[test]
fn an_empty_figure_model_fails_naming_the_field() {
    let stub = Stub::start(Plant::ModelEmpty);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/info",
            "figures[1].model: a non-empty string",
            "\"\""
        )]
    );
}

#[test]
fn a_null_figure_model_fails_as_no_string() {
    let stub = Stub::start(Plant::ModelNull);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/info",
            "figures[1].model: string",
            "null"
        )]
    );
}

#[test]
fn a_later_minor_is_still_held_to_the_figure_model_rule() {
    let stub = Stub::start(Plant::ModelEmptyOnOneThree);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/info",
            "figures[1].model: a non-empty string",
            "\"\""
        )]
    );
    assert_eq!(
        report.skipped,
        vec!["fields contract 1.3 adds beyond 1.2: this clauth knows 1.2".to_string()]
    );
}

#[test]
fn a_contract_1_0_proxy_is_not_held_to_the_1_1_figure_model() {
    let stub = Stub::start(Plant::ModelNumberOnOneZero);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(report.skipped, Vec::<String>::new());
}

#[test]
fn nothing_listening_is_an_error_naming_the_url() {
    let base = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        format!("http://{}", listener.local_addr().expect("addr"))
    };
    let (admin, key) = secrets();

    let err = check(&base, &admin, &key, &Mode::Safe).expect_err("a dead port is an error");

    assert_eq!(
        err.to_string(),
        format!("nothing answers at {base}; is the proxy running?")
    );
}

// ── the report ──────────────────────────────────────────────────────────────

#[test]
fn a_failing_report_lists_each_violation_then_the_skips_then_the_tally() {
    let report = Report {
        checks: 7,
        violations: vec![
            violation("GET /a", "status 200", "status 404"),
            violation("GET /b", "x: number", "string"),
        ],
        skipped: vec!["action claim-now: no offer item to target".to_string()],
    };

    assert_eq!(
        render(&report),
        "GET /a: expected status 200, got status 404\n\
         GET /b: expected x: number, got string\n\
         skipped: action claim-now: no offer item to target\n\
         2 of 7 checks failed\n"
    );
}

#[test]
fn a_clean_report_is_one_line() {
    let report = Report {
        checks: 7,
        violations: Vec::new(),
        skipped: Vec::new(),
    };

    assert_eq!(render(&report), "conformant: 7 checks passed\n");
}

// ── inputs ──────────────────────────────────────────────────────────────────

#[test]
fn a_base_url_is_http_or_https_with_a_host_and_nothing_after_it() {
    assert_eq!(
        base_url("http://127.0.0.1:9101/").unwrap(),
        "http://127.0.0.1:9101"
    );
    assert_eq!(
        base_url("https://proxy.example").unwrap(),
        "https://proxy.example"
    );
    for bad in [
        "zcode",
        "ftp://x",
        "http://",
        "http://a/b",
        "http://a?x=1",
        "http://u@a",
    ] {
        let err = base_url(bad).expect_err(bad);
        assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{bad}");
        assert_eq!(
            err.to_string(),
            format!("expected the proxy's base URL, like http://127.0.0.1:9101; got {bad:?}")
        );
    }
}

fn secret_file(dir: &std::path::Path, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = mode;
    path
}

#[test]
fn a_secret_file_loses_its_trailing_newline_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let path = secret_file(dir.path(), "t", "clp_abc\n", 0o600);

    assert_eq!(read_secret_file(&path, "key").unwrap().expose(), "clp_abc");
}

#[test]
fn a_secret_file_holding_whitespace_or_nothing_is_refused_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let inner = secret_file(dir.path(), "inner", "clp_a b\n", 0o600);
    let empty = secret_file(dir.path(), "empty", "\n", 0o600);

    let err = read_secret_file(&inner, "key").unwrap_err();
    assert!(err.downcast_ref::<crate::UsageError>().is_some());
    assert_eq!(
        err.to_string(),
        format!("key file {inner:?} holds whitespace or control characters")
    );
    let err = read_secret_file(&empty, "key").unwrap_err();
    assert_eq!(err.to_string(), format!("key file {empty:?} is empty"));
}

#[cfg(unix)]
#[test]
fn a_secret_file_other_users_can_read_is_refused_naming_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let path = secret_file(dir.path(), "t", "adm", 0o644);

    let err = read_secret_file(&path, "admin token").unwrap_err();

    assert!(err.downcast_ref::<crate::UsageError>().is_some());
    assert_eq!(
        err.to_string(),
        format!("admin token file {path:?} is accessible by other users; run `chmod 600` on it")
    );
}

#[test]
fn a_missing_secret_file_is_refused_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent");

    let err = read_secret_file(&path, "key").unwrap_err();

    assert!(err.downcast_ref::<crate::UsageError>().is_some());
    assert_eq!(
        err.to_string(),
        format!("cannot read key file {path:?} (entity not found)")
    );
}

// ── the target ──────────────────────────────────────────────────────────────

/// Registry rows written as a hand-edited `proxies.toml` would hold them.
fn register(rows: &[(&str, u16)]) {
    let text: String = rows
        .iter()
        .map(|(service, port)| format!("[{service}]\nport = {port}\nenabled = true\n"))
        .collect();
    let path = crate::proxy::registry_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn profile(name: &str, base_url: Option<&str>, key: Option<&str>) -> crate::profile::Profile {
    crate::profile::Profile::new(
        name.to_string(),
        base_url.map(str::to_string),
        key.map(str::to_string),
    )
}

/// No profile read: the URL form and a flag-given key never need one.
fn no_profiles() -> Result<Vec<crate::profile::Profile>> {
    panic!("the profiles were read")
}

/// A registered `zcode` on 9101 with its minted admin token, answered back.
fn registered_zcode() -> String {
    register(&[("zcode", 9101)]);
    let zcode = crate::proxy::Service::parse("zcode").unwrap();
    crate::proxy::ensure_proxy_token(&zcode)
        .unwrap()
        .expose()
        .to_string()
}

fn resolved(target: Result<Target>) -> Result<(String, String, String), String> {
    target
        .map(|t| {
            (
                t.base,
                t.admin.expose().to_string(),
                t.key.expose().to_string(),
            )
        })
        .map_err(|e| e.to_string())
}

#[test]
fn a_url_target_takes_both_files_and_refuses_naming_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let (admin, key) = secret_files(dir.path());

    assert_eq!(
        resolved(resolve_target(
            "http://127.0.0.1:9101/",
            Some(&admin),
            Some(&key),
            no_profiles
        )),
        Ok((
            "http://127.0.0.1:9101".to_string(),
            ADMIN.to_string(),
            KEY.to_string()
        ))
    );
    assert_eq!(
        resolved(resolve_target(
            "http://127.0.0.1:9101",
            None,
            Some(&key),
            no_profiles
        )),
        Err(
            "checking a proxy by URL needs --admin-token-file, the file holding its admin token"
                .to_string()
        )
    );
    assert_eq!(
        resolved(resolve_target("https://proxy.example", Some(&admin), None, no_profiles)),
        Err("checking a proxy by URL needs --key-file, the file holding an inference key of one of its accounts".to_string())
    );
}

/// A value that is neither a URL nor a service is refused naming both
/// shapes, before any registry or file is read.
#[test]
fn a_target_that_is_neither_shape_is_refused_naming_both() {
    for target in [
        "127.0.0.1:9101",
        "ftp://x",
        "HTTP://127.0.0.1:9101",
        "Zcode",
    ] {
        let err = resolve_target(target, None, None, no_profiles)
            .err()
            .expect("refused");
        assert!(
            err.downcast_ref::<crate::UsageError>().is_some(),
            "{target}"
        );
        assert_eq!(
            err.to_string(),
            format!(
                "expected a proxy's base URL, like http://127.0.0.1:9101, or the service of a proxy registered with `clauth proxy enable`, like zcode; got {target:?}"
            )
        );
    }
}

/// The service form carries the registered service into the check, which
/// holds `/health` to it; the URL form carries none.
#[test]
fn only_the_service_form_carries_the_service_to_expect() {
    let _home = crate::testutil::HomeSandbox::new();
    registered_zcode();
    let dir = tempfile::tempdir().unwrap();
    let (admin, key) = secret_files(dir.path());
    let service_of = |target: Result<Target>| {
        target
            .map(|t| t.service.map(|s| s.as_str().to_string()))
            .map_err(|e| e.to_string())
    };

    assert_eq!(
        service_of(resolve_target(
            "zcode",
            Some(&admin),
            Some(&key),
            no_profiles
        )),
        Ok(Some("zcode".to_string()))
    );
    assert_eq!(
        service_of(resolve_target(
            "http://127.0.0.1:9101",
            Some(&admin),
            Some(&key),
            no_profiles
        )),
        Ok(None)
    );
}

/// A registered proxy whose token file is missing (a hand-written row, a
/// deleted file) is refused naming the command that mints it.
#[test]
fn a_registered_proxy_without_its_token_file_is_refused_naming_enable() {
    let _home = crate::testutil::HomeSandbox::new();
    register(&[("zcode", 9101)]);
    let path =
        crate::proxy::admin_token_path(&crate::proxy::Service::parse("zcode").unwrap()).unwrap();

    let err = resolve_target("zcode", None, None, no_profiles)
        .err()
        .expect("refused");

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        format!(
            "proxy \"zcode\" has no admin token file {path:?}; run `clauth proxy enable zcode` to mint it"
        )
    );
}

#[test]
fn an_unregistered_service_is_refused_naming_enable() {
    let _home = crate::testutil::HomeSandbox::new();
    register(&[("qwen", 9102)]);
    assert_eq!(
        resolved(resolve_target("zcode", None, None, no_profiles)),
        Err(
            "no proxy \"zcode\" is registered; register it with `clauth proxy enable zcode` \
             (`clauth proxy list` names every proxy found)"
                .to_string()
        )
    );
}

/// A refused row is refused by `check <service>` before any profile read,
/// with the one text a write refuses with.
#[test]
fn a_check_of_a_refused_row_refuses_naming_the_row() {
    let _home = crate::testutil::HomeSandbox::new();
    register(&[("zcode", 9101)]);
    let path = crate::proxy::registry_path().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("{text}\n[qwen]\nport = 65000\nenabeld = true\n"),
    )
    .unwrap();

    let err = resolve_target("qwen", None, None, no_profiles)
        .err()
        .expect("refused");

    assert_eq!(
        err.to_string(),
        format!(
            "invalid proxy registry {}: row \"qwen\": unknown key \"enabeld\"",
            path.display()
        )
    );
}

/// A hand-set admin token under the floor is refused by `check <service>`
/// with the same reason `enable` gives: the one rule both call.
#[cfg(unix)]
#[test]
fn a_short_admin_token_is_refused_by_check_naming_the_same_reason() {
    let _home = crate::testutil::HomeSandbox::new();
    register(&[("zcode", 9101)]);
    let zcode = crate::proxy::Service::parse("zcode").unwrap();
    let path = crate::proxy::admin_token_path(&zcode).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "short\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let err = resolve_target("zcode", None, None, no_profiles)
        .err()
        .expect("refused");

    assert_eq!(
        err.to_string(),
        format!(
            "admin token file {:?} is shorter than 32 characters; delete the file and clauth mints a new one",
            path
        )
    );
}

/// `check <service> --admin-token-file <own file>` reads the user's own file
/// with the plain reader: a file holding a space is refused by the reader's
/// own message, never the mint advice that belongs to clauth's token file
/// (clauth mints only `admin_token_path(service)`, never the file handed over).
#[test]
fn an_explicit_admin_token_file_is_read_with_the_plain_reader() {
    let _home = crate::testutil::HomeSandbox::new();
    register(&[("zcode", 9101)]);
    let dir = tempfile::tempdir().unwrap();
    let admin = secret_file(dir.path(), "my-token", "adm has a space\n", 0o600);
    let key = secret_file(dir.path(), "key", &format!("{KEY}\n"), 0o600);

    let err = resolve_target("zcode", Some(&admin), Some(&key), no_profiles)
        .err()
        .expect("refused");

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        format!("admin token file {admin:?} holds whitespace or control characters"),
        "the reader's own message, never the mint advice"
    );
}

/// clauth's own token file failing with `PermissionDenied` (owner-read off)
/// names `chmod 600` as the fix, never the delete advice that would throw the
/// token away.
#[cfg(unix)]
#[test]
fn an_unreadable_admin_token_names_chmod_600_as_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let path = secret_file(dir.path(), "t", "x", 0o000);

    let err = read_admin_token_optional(&path).unwrap_err();

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        format!("cannot read admin token file {path:?} (permission denied); run `chmod 600` on it")
    );
}

/// A token file its owner cannot read but others can (`0o044`, `0o040`)
/// fails the read before any mode question: the refusal still names
/// `chmod 600`, which fixes both, and never stops at the bare read error.
#[cfg(unix)]
#[test]
fn an_owner_unreadable_group_readable_admin_token_names_chmod_600() {
    let dir = tempfile::tempdir().unwrap();
    for mode in [0o044, 0o040, 0o004] {
        let path = secret_file(dir.path(), &format!("t{mode:o}"), "x", mode);

        let err = read_admin_token_optional(&path).unwrap_err();

        assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
        assert_eq!(
            err.to_string(),
            format!(
                "cannot read admin token file {path:?} (permission denied); run `chmod 600` on it"
            ),
            "mode {mode:o}"
        );
    }
}

/// Every admin-token read refusal is a usage error, so `enable` and
/// `check <service>` exit 2 on bad input the way `read_secret_file`'s other
/// refusals do.
#[cfg(unix)]
#[test]
fn every_read_admin_token_refusal_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    for (name, text, mode) in [
        ("empty", "\n", 0o600),
        ("space", "adm has a space\n", 0o600),
        ("short", "short\n", 0o600),
        ("mode", "adm-0123456789abcdef0123456789abcdef\n", 0o644),
        ("unreadable", "x", 0o000),
    ] {
        let path = secret_file(dir.path(), name, text, mode);
        let err = read_admin_token_optional(&path).unwrap_err();
        assert!(
            err.downcast_ref::<crate::UsageError>().is_some(),
            "{name}: {err:#}"
        );
    }
}

/// The refusal mapping never doubles the `chmod 600` clause, and each order
/// is decided by which half refused: a READ refusal decides the owner-read
/// bit first (a `0o044` file names `chmod 600` once off the read error),
/// while a VALIDATION refusal keeps the others-can-read question first (its
/// message passes through unchanged, never gaining a second clause). The
/// doubled shape needs a read that succeeds with the owner-read bit off —
/// root only — so the mapping is pinned directly with its inputs.
#[cfg(unix)]
#[test]
fn the_admin_token_refusal_mapping_decides_each_order_once() {
    let dir = tempfile::tempdir().unwrap();
    let open = secret_file(dir.path(), "open", "x", 0o044);

    let e = crate::usage_error(format!(
        "admin token file {open:?} is accessible by other users; run `chmod 600` on it"
    ));
    let fixed = refuse_admin_token_validation(&open, e);
    assert_eq!(
        fixed.to_string(),
        format!("admin token file {open:?} is accessible by other users; run `chmod 600` on it")
    );

    let e = crate::usage_error(format!(
        "cannot read admin token file {open:?} (permission denied)"
    ));
    let fixed = refuse_admin_token_read(&open, e);
    assert_eq!(
        fixed.to_string(),
        format!("cannot read admin token file {open:?} (permission denied); run `chmod 600` on it")
    );
}

/// The validation branch of the admin-token rule routes through the
/// validation refusal, never the read refusal. The two refusals differ only
/// on a file that is both owner-unreadable and others-readable (`0o044`): the
/// validation error already names `chmod 600` (the others-readable rule runs
/// first inside `validate_secret_text`), so the read refusal would append the
/// advice a second time. A non-root process cannot read a `0o044` file, so
/// the test drives the post-read seam with the text it wrote, over the same
/// path the mode checks read.
#[cfg(unix)]
#[test]
fn a_whitespace_admin_token_open_to_others_names_chmod_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = secret_file(dir.path(), "t", "adm has a space\n", 0o044);

    let err = admin_token_from_text(&path, "adm has a space\n".to_string()).unwrap_err();

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        format!("admin token file {path:?} is accessible by other users; run `chmod 600` on it"),
        "one chmod clause: the validation refusal, never the read refusal's doubled advice"
    );
}

/// A token path that is a symlink loop fails the read before any mode
/// question: the refusal is the bare read error, never `chmod 600`, which
/// cannot fix a path that does not resolve.
#[cfg(unix)]
#[test]
fn a_symlink_loop_token_names_the_bare_read_refusal_not_chmod() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop");
    std::os::unix::fs::symlink("loop", &path).unwrap();

    let err = read_admin_token_optional(&path).unwrap_err();

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    let read_err = std::fs::read_to_string(&path).expect_err("the loop does not read");
    assert_eq!(
        err.to_string(),
        format!(
            "cannot read admin token file {path:?} ({})",
            read_err.kind()
        ),
        "the whole refusal is the bare read error, never `chmod 600`"
    );
}

/// The service form: the bind from the row, the token from its file, and
/// the key of the one profile whose base_url is the bind, whatever loopback
/// spelling or trailing slash it carries; another port, `https` or a path
/// is no match.
#[test]
fn a_service_target_takes_its_row_token_and_the_one_profile_on_its_bind() {
    let _home = crate::testutil::HomeSandbox::new();
    let token = registered_zcode();

    for url in [
        "http://127.0.0.1:9101",
        "http://127.0.0.1:9101/",
        "http://localhost:9101",
        "http://[::1]:9101",
        "HTTP://LOCALHOST:9101/",
    ] {
        let profiles = vec![
            profile("on-bind", Some(url), Some("clp_on-bind")),
            profile("other-port", Some("http://127.0.0.1:9102"), Some("clp_x")),
            profile("tls", Some("https://127.0.0.1:9101"), Some("clp_x")),
            profile("path", Some("http://127.0.0.1:9101/v1"), Some("clp_x")),
            profile("remote", Some("http://10.0.0.1:9101"), Some("clp_x")),
            profile("signed-port", Some("http://127.0.0.1:+9101"), Some("clp_x")),
            profile("oauth", None, None),
        ];
        assert_eq!(
            resolved(resolve_target("zcode", None, None, || Ok(profiles))),
            Ok((
                "http://127.0.0.1:9101".to_string(),
                token.clone(),
                "clp_on-bind".to_string()
            )),
            "{url}"
        );
    }
}

#[test]
fn several_profiles_on_the_bind_are_refused_naming_each() {
    let _home = crate::testutil::HomeSandbox::new();
    registered_zcode();
    let profiles = vec![
        profile("a", Some("http://127.0.0.1:9101"), Some("clp_a")),
        profile("b", Some("http://localhost:9101/"), Some("clp_b")),
    ];
    assert_eq!(
        resolved(resolve_target("zcode", None, None, || Ok(profiles))),
        Err("profiles \"a\", \"b\" all point at proxy \"zcode\" (http://127.0.0.1:9101); pass --key-file with the key to check with".to_string())
    );
}

#[test]
fn no_profile_on_the_bind_is_refused_naming_key_file() {
    let _home = crate::testutil::HomeSandbox::new();
    registered_zcode();
    let profiles = vec![profile("x", Some("http://127.0.0.1:9102"), Some("clp_x"))];
    assert_eq!(
        resolved(resolve_target("zcode", None, None, || Ok(profiles))),
        Err("no profile's base_url is proxy \"zcode\"'s bind http://127.0.0.1:9101; pass --key-file with an inference key of one of its accounts".to_string())
    );
    let keyless = vec![profile("bare", Some("http://127.0.0.1:9101"), None)];
    assert_eq!(
        resolved(resolve_target("zcode", None, None, || Ok(keyless))),
        Err(
            "profile \"bare\" points at proxy \"zcode\" and holds no api key; pass --key-file"
                .to_string()
        )
    );
}

/// `--key-file` and `--admin-token-file` override the registry's picks: the
/// profiles are never read, so several on the bind refuse nothing.
#[test]
fn the_flags_override_a_service_targets_picks() {
    let _home = crate::testutil::HomeSandbox::new();
    registered_zcode();
    let dir = tempfile::tempdir().unwrap();
    let (admin, key) = secret_files(dir.path());

    assert_eq!(
        resolved(resolve_target(
            "zcode",
            Some(&admin),
            Some(&key),
            no_profiles
        )),
        Ok((
            "http://127.0.0.1:9101".to_string(),
            ADMIN.to_string(),
            KEY.to_string()
        ))
    );
}

#[test]
fn a_secret_never_prints_through_debug() {
    let secret = Secret::new("clp_abc".to_string());

    assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
}

// ── round-1 review pins ─────────────────────────────────────────────────────

#[test]
fn a_proxy_echoing_a_credential_never_gets_it_printed() {
    let stub = Stub::start(Plant::EchoSecrets);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![
            violation("GET /health", "status: \"ok\"", "\"<redacted>\""),
            violation(
                "GET /clauth/v1/info",
                "version: \"0.0.1\"",
                "\"<redacted>\""
            ),
        ]
    );
    let printed = render(&report);
    assert!(!printed.contains(ADMIN), "{printed}");
    assert!(!printed.contains(KEY), "{printed}");
}

#[test]
fn a_proxy_supplied_control_character_is_escaped_in_the_report() {
    let stub = Stub::start(Plant::EscapeInSettingKey);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/accounts",
            "accounts[0].settings.\\u{001b}[2J: a declared account setting",
            "undeclared"
        )]
    );
    assert!(!render(&report).contains('\u{1b}'));
}

#[test]
fn escape_writes_every_control_and_bidi_character_as_its_code() {
    assert_eq!(
        "a\u{1b}b\u{202e}c\u{85}d\u{2066}e"
            .chars()
            .map(escape_char)
            .collect::<String>(),
        "a\\u{001b}b\\u{202e}c\\u{0085}d\\u{2066}e"
    );
    assert_eq!(escape_char('é'), "é");
    assert_eq!(
        "plain text é".chars().map(escape_char).collect::<String>(),
        "plain text é",
        "a space and every ASCII letter pass through unescaped"
    );
}

#[test]
fn numbers_compare_by_value_and_nothing_else_does() {
    assert!(same(&json!(4.0), &json!(4)));
    assert!(!same(&json!(4.5), &json!(4)));
    assert!(!same(&json!("4"), &json!(4)));
}

/// A cut that would land inside an escape backs off to the whole piece: the 76
/// a's leave the `\u{001b}` piece 8 cells it cannot fit in the 78 content
/// cells, so the piece is left out whole and the dots close the line — never a
/// half-spelling the value never had.
#[test]
fn printable_never_splits_an_escape_at_its_cut() {
    let (admin, key) = secrets();
    let checker = Checker {
        base: "http://127.0.0.1:1",
        expected_service: None,
        admin: &admin,
        key: &key,
        secrets: Vec::new(),
        control: control_agent(CONTROL_TIMEOUT),
        inference: control_agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };
    let text = format!("{}{}", "a".repeat(76), "\u{1b}bbbbbbbbbb");
    assert_eq!(
        checker.printable(&text, REPR_MAX),
        format!("{}...", "a".repeat(76)),
        "the escape is never cut in half"
    );
}

/// The cap counts terminal cells, so `❤️` — a heart and its presentation
/// selector, one grapheme the terminal draws two cells wide — spends both: 80
/// of them overrun a `REPR_MAX` line, and the cut keeps 39 whole pairs.
#[test]
fn printable_measures_an_emoji_presentation_sequence_as_its_drawn_cells() {
    let (admin, key) = secrets();
    let checker = Checker {
        base: "http://127.0.0.1:1",
        expected_service: None,
        admin: &admin,
        key: &key,
        secrets: Vec::new(),
        control: control_agent(CONTROL_TIMEOUT),
        inference: control_agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };

    let text = "\u{2764}\u{fe0f}".repeat(80);
    assert_eq!(
        checker.printable(&text, REPR_MAX),
        format!("{}...", "\u{2764}\u{fe0f}".repeat(39)),
        "39 two-cell pairs take 78 cells and the dots close the 81-cell line"
    );
}

/// A run of zero-width characters is bounded the way a run of drawn ones is:
/// each still spends a character of the budget, so a pasted run can never
/// carry unbounded bytes into a report line, while a short value stays whole.
#[test]
fn printable_bounds_a_run_of_zero_width_characters() {
    let (admin, key) = secrets();
    let checker = Checker {
        base: "http://127.0.0.1:1",
        expected_service: None,
        admin: &admin,
        key: &key,
        secrets: Vec::new(),
        control: control_agent(CONTROL_TIMEOUT),
        inference: control_agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };

    let text = format!("{}hi", "\u{200b}".repeat(1000));
    assert_eq!(
        checker.printable(&text, REPR_MAX),
        format!("{}...", "\u{200b}".repeat(78)),
        "78 zero-width characters take the content budget, then the dots"
    );
    assert_eq!(
        checker.printable("a\u{200b}b", REPR_MAX),
        "a\u{200b}b",
        "a short run stays whole"
    );
}

/// A cap bounds the characters an escape body carries as well as its cells:
/// one grapheme of ~250 KB of combining marks passes any cell measure whole,
/// so the char budget stops it and the dots close the line.
#[test]
fn printable_bounds_a_single_oversized_grapheme() {
    let (admin, key) = secrets();
    let checker = Checker {
        base: "http://127.0.0.1:1",
        expected_service: None,
        admin: &admin,
        key: &key,
        secrets: Vec::new(),
        control: control_agent(CONTROL_TIMEOUT),
        inference: control_agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };

    let marks = format!("a{}", "\u{301}".repeat(125_000));
    assert_eq!(
        checker.printable(&marks, REPR_MAX),
        "...",
        "one 250 KB cluster overruns the character budget whole"
    );
}

#[test]
fn printable_keeps_values_whole_through_the_budget_and_cuts_past_it() {
    let (admin, key) = secrets();
    let checker = Checker {
        base: "http://127.0.0.1:1",
        expected_service: None,
        admin: &admin,
        key: &key,
        secrets: Vec::new(),
        control: control_agent(CONTROL_TIMEOUT),
        inference: control_agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };

    assert_eq!(checker.printable("aaaa", 5), "aaaa", "max-1 fits whole");
    assert_eq!(checker.printable("aaaaa", 5), "aaaaa", "max fits whole");
    assert_eq!(checker.printable("aaaaaa", 5), "aaaaaa", "max+1 fits whole");
    assert_eq!(
        checker.printable("aaaaaaa", 5),
        "aaa...",
        "max+2 cuts to max-2 chars + dots"
    );
    // Multi-byte chars count once and slice on char boundaries.
    assert_eq!(checker.printable("ééé", 5), "ééé", "multi-byte fits whole");
    assert_eq!(
        checker.printable("ééééééé", 5),
        "ééé...",
        "multi-byte cuts whole chars"
    );
    // Too small a budget shows dots only, never content.
    assert_eq!(checker.printable("aaaa", 0), ".", "max 0 shows one dot");
    assert_eq!(checker.printable("aaaa", 1), "..", "max 1 shows two dots");
}

#[test]
fn a_time_must_be_rfc_3339_in_utc() {
    assert!(Kind::Time.admits(&json!("2026-09-29T12:00:00Z")));
    assert!(Kind::Time.admits(&json!("2026-09-29T12:00:00+00:00")));
    assert!(!Kind::Time.admits(&json!("2026-09-29T14:00:00+02:00")));
    assert!(!Kind::Time.admits(&json!("2026-09-29 12:00:00")));
}

#[test]
fn a_cors_header_fails_naming_the_route() {
    let stub = Stub::start(Plant::CorsOnHealth);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /health",
            "no Access-Control-Allow-* header",
            "access-control-allow-origin"
        )]
    );
}

fn destructive_report(plant: Plant) -> Report {
    let stub = Stub::start(plant);
    let (admin, key) = secrets();
    check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: ACCOUNT.to_string(),
        },
    )
    .expect("check runs")
}

#[test]
fn a_remint_that_leaves_the_old_key_working_fails_naming_it() {
    assert_eq!(
        destructive_report(Plant::RemintKeepsOldKey).violations,
        vec![violation(
            "POST /v1/messages (the key file's key after the re-mint)",
            "status 401",
            "status 200"
        )]
    );
}

#[test]
fn a_delete_that_leaves_the_key_working_fails_naming_it() {
    assert_eq!(
        destructive_report(Plant::DeleteKeepsKey).violations,
        vec![violation(
            "POST /v1/messages (re-minted key, account deleted)",
            "status 401",
            "status 200"
        )]
    );
}

#[test]
fn a_cancelled_flow_that_still_reads_back_fails_naming_it() {
    let stub = Stub::start(Plant::CancelledFlowReadsFailed);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/accounts/login/{flow} (cancelled)",
            "status 404",
            "status 200"
        )]
    );
}

fn secret_files(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    (
        secret_file(dir, "admin", &format!("{ADMIN}\n"), 0o600),
        secret_file(dir, "key", &format!("{KEY}\n"), 0o600),
    )
}

#[test]
fn run_succeeds_on_a_conformant_proxy() {
    let stub = Stub::start(Plant::None);
    let dir = tempfile::tempdir().unwrap();
    let (admin, key) = secret_files(dir.path());

    run(&format!("{}/", stub.base), Some(&admin), Some(&key), None)
        .expect("a conformant proxy exits 0");
}

/// `clauth proxy check <service>` runs the same check on a registered proxy,
/// with the admin token from its token file and the key of the one profile
/// on its bind.
/// A stub proxy registered as `stub`, with its admin token in the registry's
/// token file and one profile `st` on its bind holding the stub's key.
fn registered_stub(plant: Plant) -> Stub {
    let stub = Stub::start(plant);
    let port = stub
        .base
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .expect("the stub's port");
    register(&[("stub", port)]);
    let registered = crate::proxy::Service::parse("stub").unwrap();
    let token = crate::proxy::admin_token_path(&registered).unwrap();
    std::fs::create_dir_all(token.parent().unwrap()).unwrap();
    secret_file(token.parent().unwrap(), "clauth-admin-token", ADMIN, 0o600);
    crate::profile::save_profile(&crate::profile::Profile::new(
        "st".to_string(),
        Some(format!("{}/", stub.base)),
        Some(KEY.to_string()),
    ))
    .unwrap();
    crate::testutil::register_names(&["st"]);
    stub
}

#[test]
fn run_checks_a_registered_proxy_by_its_service() {
    let _home = crate::testutil::HomeSandbox::new();
    let _stub = registered_stub(Plant::None);

    run("stub", None, None, None).expect("a conformant registered proxy exits 0");
}

/// `run`'s service form holds `/health` to the registered service: a proxy
/// answering as another is stopped there, before its admin token or a key
/// reaches it, and the run exits 1 on that one departure.
#[test]
fn run_stops_a_registered_proxy_answering_as_another_service_at_health() {
    let _home = crate::testutil::HomeSandbox::new();
    let stub = registered_stub(Plant::OtherService);

    let (base, report) = check_target("stub", None, None, None).expect("check runs");

    assert_eq!(
        (base, report.violations),
        (
            stub.base.clone(),
            vec![violation(
                "GET /health",
                "service: \"stub\", the registered proxy",
                "\"other-proxy\""
            )]
        )
    );
    assert_eq!(stub.state().requests, vec!["GET /health".to_string()]);

    let again = registered_stub(Plant::OtherService);
    let err = run("stub", None, None, None).expect_err("a departing proxy exits 1");
    assert_eq!(
        err.to_string(),
        format!("{} departs from contract v1 in 1 place", again.base)
    );
    assert_eq!(again.state().requests, vec!["GET /health".to_string()]);
}

/// A registered proxy's `/health` naming another service stops the check
/// there, naming both: no admin token, key or inference request reaches
/// whatever answers on the recorded port.
#[test]
fn a_registered_proxy_answering_as_another_service_stops_at_health() {
    let stub = Stub::start(Plant::OtherService);
    let (admin, key) = secrets();
    let stub_service = crate::proxy::Service::parse("stub").unwrap();

    let report =
        check_registered(&stub.base, &admin, &key, &Mode::Safe, &stub_service).expect("check runs");

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /health",
            "service: \"stub\", the registered proxy",
            "\"other-proxy\""
        )]
    );
    assert_eq!(stub.state().requests, vec!["GET /health".to_string()]);
    assert_eq!(stub.state().inferences, 0);
}

/// A URL holds no expectation: the same answer is checked on.
#[test]
fn a_proxy_checked_by_url_may_name_any_service() {
    let stub = Stub::start(Plant::OtherService);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    assert!(
        report.violations.iter().all(|v| v.route != "GET /health"),
        "{:?}",
        report.violations
    );
    assert_eq!(stub.state().inferences, 1, "the run went on to inference");
}

/// `--destructive` on a registered proxy without `--key-file` is refused
/// before anything is sent: the profile's key need not be the named
/// account's, and the proxy is the user's live one.
#[test]
fn destructive_on_a_registered_proxy_needs_key_file() {
    let _home = crate::testutil::HomeSandbox::new();
    let port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        listener.local_addr().expect("addr").port()
    };
    register(&[("zcode", port)]);
    crate::proxy::ensure_proxy_token(&crate::proxy::Service::parse("zcode").unwrap()).unwrap();
    crate::profile::save_profile(&crate::profile::Profile::new(
        "zc".to_string(),
        Some(format!("http://127.0.0.1:{port}")),
        Some(KEY.to_string()),
    ))
    .unwrap();
    crate::testutil::register_names(&["zc"]);

    let err = run("zcode", None, None, Some(ACCOUNT.to_string())).expect_err("refused");

    assert!(err.downcast_ref::<crate::UsageError>().is_some(), "{err:#}");
    assert_eq!(
        err.to_string(),
        "--destructive needs --key-file holding the named account's key: a registered proxy's key is picked from a profile, which need not be that account's, and the proxy is your live one"
    );
}

#[test]
fn run_fails_naming_how_many_departures_a_proxy_makes() {
    let stub = Stub::start(Plant::UsedAsString);
    let dir = tempfile::tempdir().unwrap();
    let (admin, key) = secret_files(dir.path());

    let err =
        run(&stub.base, Some(&admin), Some(&key), None).expect_err("a departing proxy exits 1");

    assert!(
        err.downcast_ref::<crate::UsageError>().is_none(),
        "a departure is no usage error"
    );
    assert_eq!(
        err.to_string(),
        format!("{} departs from contract v1 in 2 places", stub.base)
    );
}

// ── round-2 review pins ─────────────────────────────────────────────────────

fn safe_report(plant: Plant) -> (Report, Stub) {
    let stub = Stub::start(plant);
    let (admin, key) = secrets();
    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");
    (report, stub)
}

#[test]
fn a_rebind_probe_the_proxy_wrongly_opens_is_named_and_cancelled() {
    let (report, stub) = safe_report(Plant::IgnoreRebindAccount);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /clauth/v1/accounts/login (re-bind, unknown id)",
            "status 404",
            "status 201"
        )]
    );
    assert!(stub.state().flows.is_empty(), "no flow is left polling");
}

#[test]
fn a_login_opened_without_the_token_is_named_and_cancelled() {
    let (report, stub) = safe_report(Plant::LoginWithoutToken);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /clauth/v1/accounts/login (no token)",
            "status 401",
            "status 201"
        )]
    );
    assert!(stub.state().flows.is_empty(), "no flow is left polling");
}

#[test]
fn a_login_answered_with_the_wrong_status_is_still_cancelled() {
    let (report, stub) = safe_report(Plant::LoginAnswers200);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /clauth/v1/accounts/login",
            "status 201",
            "status 200"
        )]
    );
    assert!(stub.state().flows.is_empty(), "no flow is left polling");
}

#[test]
fn a_proxy_echoing_the_reminted_key_never_gets_it_printed() {
    let report = destructive_report(Plant::EchoFreshKey);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /v1/messages (re-minted key)",
            "type: \"message\"",
            "\"<redacted>\""
        )]
    );
    assert!(!render(&report).contains("clp_stub-key-1"));
}

#[test]
fn a_json_answer_labelled_otherwise_fails_naming_the_route() {
    let (report, _stub) = safe_report(Plant::TextConfig);

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/config",
            "content-type: application/json",
            "\"text/plain\""
        )]
    );
}

#[test]
fn a_health_body_at_the_cap_fails() {
    let (report, _stub) = safe_report(Plant::BigHealth);

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /health",
            "a body under 4096 bytes",
            "4096 bytes"
        )]
    );
}

#[test]
fn a_health_answer_slower_than_the_daemon_probe_fails() {
    let (report, _stub) = safe_report(Plant::SlowHealth);

    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let only = &report.violations[0];
    assert_eq!(only.route, "GET /health");
    assert_eq!(
        only.expected,
        "an answer inside the daemon probe's bounds (4 s to connect, 2 s to answer)"
    );
    assert!(only.got.starts_with("no answer ("), "{}", only.got);
}

#[test]
fn a_proxy_accepting_any_bearer_key_fails_naming_it() {
    let (report, _stub) = safe_report(Plant::BearerAnyKey);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /v1/messages (unknown bearer key)",
            "status 401",
            "status 200"
        )]
    );
}

#[test]
fn a_destructive_run_skips_a_setting_the_account_cannot_use_and_says_why() {
    let report = destructive_report(Plant::StartPlanAccount);

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(
        report.skipped,
        vec![
            "setting off_peak: inactive under the account's current plan".to_string(),
            "setting timezone: no other valid value to set".to_string(),
        ]
    );
}

/// A key that is not an id form is quoted, so the escape text a proxy writes
/// out reads apart from the escape of the control character it spells.
#[test]
fn a_skip_line_quotes_an_active_key_that_is_not_an_id() {
    let literal = destructive_report(Plant::LiteralEscapeInActiveKey);
    assert_eq!(
        literal.skipped,
        vec![
            "setting off_peak: inactive under the account's current \"a\\\\u{001b}b\"".to_string(),
            "setting timezone: no other valid value to set".to_string(),
        ]
    );
    let real = destructive_report(Plant::EscapeInActiveKey);
    assert_eq!(
        real.skipped,
        vec![
            "setting off_peak: inactive under the account's current \"a\\u001bb\"".to_string(),
            "setting timezone: no other valid value to set".to_string(),
        ]
    );
}

#[test]
fn a_destructive_account_no_route_can_name_is_skipped_not_refused() {
    let stub = Stub::start(Plant::UppercaseAccountId);
    let (admin, key) = secrets();

    let report = check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: "Acct-1".to_string(),
        },
    )
    .expect("a listed account is no usage error");

    assert_eq!(
        report.violations,
        vec![
            violation(
                "GET /clauth/v1/accounts",
                "accounts[0].id: id ([a-z0-9][a-z0-9._-]{0,63})",
                "\"Acct-1\""
            ),
            violation(
                "GET /clauth/v1/usage",
                "accounts[0].account: an account GET /accounts lists",
                "\"Acct-1\""
            ),
        ]
    );
    assert_eq!(
        report.skipped,
        vec![
            "--destructive: account \"Acct-1\" has an id outside the contract's id form, so no route can name it".to_string(),
            "GET /clauth/v1/usage?account={id}: the proxy holds no account".to_string(),
        ]
    );
    assert_eq!(
        stub.state().mutations,
        vec![
            "POST /accounts/login".to_string(),
            "DELETE /accounts/login/f_1".to_string()
        ],
        "no destructive request ran"
    );
}

#[test]
fn a_cors_header_on_a_control_route_fails_naming_each_answer() {
    let (report, _stub) = safe_report(Plant::CorsOnInfo);

    let cors = |route: &str| {
        violation(
            route,
            "no Access-Control-Allow-* header",
            "access-control-allow-credentials",
        )
    };
    assert_eq!(
        report.violations,
        vec![
            cors("GET /clauth/v1/info (no token)"),
            cors("GET /clauth/v1/info (wrong token)"),
            cors("GET /clauth/v1/info (inference key)"),
            cors("GET /clauth/v1/info"),
        ]
    );
}

#[test]
fn an_event_stream_that_never_sends_its_head_is_named_as_such() {
    let (report, _stub) = safe_report(Plant::SseNoHead);

    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let only = &report.violations[0];
    assert_eq!(only.route, "GET /clauth/v1/events");
    assert_eq!(
        only.expected,
        "a response head within 20 s (an event stream sends its head before its first event)"
    );
    assert!(only.got.starts_with("no answer ("), "{}", only.got);
}

#[test]
fn a_numeric_setting_at_its_bound_moves_to_a_different_value() {
    let decl = SettingDecl {
        key: "level".to_string(),
        scope: crate::proxy_account::Scope::Account,
        label: "level".to_string(),
        hint: String::new(),
        typ: "number".to_string(),
        default: Some(json!(0.0)),
        options: Vec::new(),
        min: Some(0.0),
        max: Some(10.0),
        step: Some(1.0),
        unit: None,
        active_when: None,
        inactive_hint: None,
        restart: false,
    };

    assert_eq!(decl.alternative(&json!(0)), Some(json!(10.0)));
    assert_eq!(decl.alternative(&json!(10)), Some(json!(0.0)));
}

#[test]
fn a_secret_is_redacted_raw_and_json_escaped() {
    let (admin, key) = (
        Secret::new("adm\"quoted\\slash".to_string()),
        Secret::new(KEY.to_string()),
    );
    let checker = Checker {
        base: "http://127.0.0.1:1",
        expected_service: None,
        admin: &admin,
        key: &key,
        secrets: vec![admin.expose().to_string(), key.expose().to_string()],
        control: control_agent(CONTROL_TIMEOUT),
        inference: control_agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };

    assert_eq!(
        checker.printable(&repr(&json!(admin.expose())), 200),
        "\"<redacted>\""
    );
    assert_eq!(
        checker.printable(&format!("x {} y", admin.expose()), 200),
        "x <redacted> y"
    );
}

#[test]
fn a_dot_segment_flow_is_never_cancelled_by_path() {
    let answer = |flow: &str| Answer {
        status: 201,
        content_type: "application/json".to_string(),
        www_authenticate: None,
        cors: None,
        body: json!({"flow": flow}).to_string().into_bytes(),
    };

    assert_eq!(opened_flow(&answer("..")), None);
    assert_eq!(opened_flow(&answer(".")), None);
    assert_eq!(opened_flow(&answer("F_1.a~b")), Some("F_1.a~b".to_string()));
}

// ── P2 follow-ups: the contract paths the first check left unexercised ──────

#[test]
fn a_destructive_run_resets_a_setting_through_null() {
    let stub = Stub::start(Plant::None);
    let (admin, key) = secrets();

    let report = check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: ACCOUNT.to_string(),
        },
    )
    .expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert!(
        stub.state().mutations.iter().any(|m| m.ends_with("=null")),
        "a `null` patch resets a key to its default, and the run sends one: {:?}",
        stub.state().mutations
    );
}

#[test]
fn a_null_reset_that_keeps_the_value_fails_naming_it() {
    let report = destructive_report(Plant::NullResetKeeps);

    assert!(
        !report.violations.is_empty(),
        "a reset that keeps the value is a departure"
    );
    for v in &report.violations {
        assert!(
            v.route
                .starts_with("PATCH /clauth/v1/accounts/{id} (reset "),
            "{v:?}"
        );
    }
}

#[test]
fn a_destructive_run_resets_an_off_default_setting() {
    let stub = Stub::start(Plant::OffPeakOn);
    let (admin, key) = secrets();

    let report = check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: ACCOUNT.to_string(),
        },
    )
    .expect("check runs");

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert_eq!(
        report.skipped,
        vec!["setting timezone: no other valid value to set".to_string()]
    );
    let state = stub.state();
    assert_eq!(
        state.mutations,
        [
            "POST /accounts/login",
            "DELETE /accounts/login/f_1",
            "POST /accounts/login account=acct-1",
            "DELETE /accounts/login/f_2",
            "PATCH /accounts/acct-1 plan=start-plan",
            "PATCH /accounts/acct-1 plan=null",
            "PATCH /accounts/acct-1 plan=coding-plan",
            "PATCH /accounts/acct-1 off_peak=false",
            "PATCH /accounts/acct-1 off_peak=true",
            "PATCH /accounts/acct-1 off_peak=null",
            "PATCH /accounts/acct-1 off_peak=true",
            "POST /accounts/acct-1/actions/claim-now",
            "POST /accounts/acct-1/key",
            "DELETE /accounts/acct-1",
        ]
        .map(String::from)
        .to_vec()
    );
    assert!(state.accounts.is_empty(), "the named account is consumed");
}

#[test]
fn a_paste_goes_only_to_a_poll_only_flow_which_refuses_it() {
    let (report, stub) = safe_report(Plant::PollOnly);

    assert_eq!(report.violations, Vec::<Violation>::new());
    assert!(
        stub.state()
            .requests
            .iter()
            .any(|r| r.starts_with("POST /clauth/v1/accounts/login/f_")),
        "a poll-only flow gets a paste, which it must refuse: {:?}",
        stub.state().requests
    );

    let (report, stub) = safe_report(Plant::None);
    assert_eq!(report.violations, Vec::<Violation>::new());
    assert!(
        !stub
            .state()
            .requests
            .iter()
            .any(|r| r.starts_with("POST /clauth/v1/accounts/login/f_")),
        "a flow that takes pastes never gets one: a paste reaches upstream"
    );
}

#[test]
fn a_login_start_that_omits_modes_gets_no_paste() {
    let (_report, stub) = safe_report(Plant::NoModes);

    assert!(
        !stub
            .state()
            .requests
            .iter()
            .any(|r| r.starts_with("POST /clauth/v1/accounts/login/f_")),
        "a start that omits `modes` gets no paste: {:?}",
        stub.state().requests
    );
}

#[test]
fn a_poll_only_flow_taking_a_paste_fails_naming_it() {
    let (report, stub) = safe_report(Plant::PollOnlyAcceptsPaste);

    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let only = &report.violations[0];
    assert!(
        only.route
            .starts_with("POST /clauth/v1/accounts/login/{flow}"),
        "{only:?}"
    );
    assert_eq!(
        (only.expected.as_str(), only.got.as_str()),
        ("status 409", "status 200")
    );
    assert!(stub.state().flows.is_empty(), "no flow is left polling");
}

#[test]
fn a_wrong_method_answered_404_fails_naming_405() {
    let (report, _stub) = safe_report(Plant::No405);

    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let only = &report.violations[0];
    assert_eq!(only.route, "PUT /clauth/v1/accounts");
    assert_eq!(
        (only.expected.as_str(), only.got.as_str()),
        ("status 405", "status 404")
    );
}

#[test]
fn an_oversized_control_body_served_like_any_other_fails_naming_413() {
    let (report, stub) = safe_report(Plant::NoBodyCap);

    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let only = &report.violations[0];
    assert_eq!(
        only.route,
        "PATCH /clauth/v1/accounts/{id} (oversized body)"
    );
    assert_eq!(only.expected, "status 413");
    assert!(stub.state().flows.is_empty(), "no flow is left polling");
}

#[test]
fn a_config_missing_a_declared_proxy_setting_fails_naming_it() {
    let (report, _stub) = safe_report(Plant::ConfigMissingValue);

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/config",
            "values.host_identity: the setting's current value",
            "nothing"
        )]
    );
}

#[test]
fn a_flow_opened_under_the_wrong_status_is_read_back_after_its_cancel() {
    let (report, _stub) = safe_report(Plant::LoginAnswers200KeepsFlow);

    assert_eq!(report.violations.len(), 2, "{:?}", report.violations);
    assert_eq!(
        report.violations[0],
        violation("POST /clauth/v1/accounts/login", "status 201", "status 200")
    );
    assert!(
        report.violations[1]
            .route
            .starts_with("GET /clauth/v1/accounts/login/{flow}"),
        "{:?}",
        report.violations
    );
}

#[test]
fn a_stray_flow_is_read_back_after_its_cancel() {
    let (report, _stub) = safe_report(Plant::StrayFlowKept);

    assert_eq!(report.violations.len(), 2, "{:?}", report.violations);
    assert_eq!(
        report.violations[0],
        violation(
            "POST /clauth/v1/accounts/login (re-bind, unknown id)",
            "status 404",
            "status 201"
        )
    );
    assert!(
        report.violations[1]
            .route
            .starts_with("GET /clauth/v1/accounts/login/{flow}"),
        "{:?}",
        report.violations
    );
}

#[test]
fn a_stray_flow_cancelled_with_the_wrong_status_fails_naming_it() {
    let (report, stub) = safe_report(Plant::StrayFlowDelete404);

    assert_eq!(
        report.violations,
        vec![
            violation(
                "POST /clauth/v1/accounts/login (re-bind, unknown id)",
                "status 404",
                "status 201"
            ),
            violation(
                "DELETE /clauth/v1/accounts/login/{flow} (stray flow)",
                "status 204",
                "status 404"
            ),
        ]
    );
    assert!(stub.state().flows.is_empty(), "no flow is left polling");
}

#[test]
fn a_health_transport_error_is_worded_by_its_kind() {
    let one = |plant| {
        let (report, _stub) = safe_report(plant);
        assert_eq!(
            report.violations.len(),
            1,
            "{plant:?}: {:?}",
            report.violations
        );
        let only = report.violations[0].clone();
        assert_eq!(only.route, "GET /health");
        only
    };

    let not_http = one(Plant::HealthNotHttp);
    let closed = one(Plant::HealthCloses);

    assert_eq!(not_http.expected, "bytes that are HTTP");
    assert!(
        not_http.got.starts_with("bytes that are not HTTP ("),
        "a protocol error names the bytes that arrived, never `no answer`: {}",
        not_http.got
    );
    assert_eq!(closed.expected, "a whole answer");
    assert!(
        closed.got.starts_with("the connection closed early ("),
        "an EOF/reset/abort names the connection closing, with the error: {}",
        closed.got
    );
    assert_ne!(
        not_http.expected, closed.expected,
        "two kinds, two wordings"
    );
}

#[test]
fn a_null_config_value_on_a_setting_without_a_default_fails_naming_it() {
    let (report, _stub) = safe_report(Plant::NoDefaultNull);

    assert_eq!(
        report.violations,
        vec![
            violation(
                "GET /clauth/v1/info",
                "settings[4].default: the setting's type or null",
                "nothing"
            ),
            violation(
                "GET /clauth/v1/config",
                "values.k: a valid bool value",
                "null"
            ),
        ]
    );
}

#[test]
fn an_explicit_null_default_stays_nullable() {
    let (report, _stub) = safe_report(Plant::NullDefault);

    assert_eq!(report.violations, Vec::<Violation>::new());
}

#[test]
fn a_setting_with_no_default_gets_no_null_reset() {
    let stub = Stub::start(Plant::NoDefaultReset);
    let (admin, key) = secrets();

    let report = check(
        &stub.base,
        &admin,
        &key,
        &Mode::Destructive {
            account: ACCOUNT.to_string(),
        },
    )
    .expect("check runs");

    // The contract requires `default` on every setting, so the planted one is
    // itself one departure, named once; the skipped reset adds none.
    assert_eq!(
        report.violations,
        vec![violation(
            "GET /clauth/v1/info",
            "settings[4].default: the setting's type or null",
            "nothing"
        )]
    );
    assert_eq!(
        report
            .skipped
            .iter()
            .filter(|s| s.as_str() == "setting k: no default to reset to")
            .count(),
        1,
        "{:?}",
        report.skipped
    );
    // The destructive run restores every setting it changed, default or not.
    let state = stub.state();
    let k_patches: Vec<&str> = state
        .mutations
        .iter()
        .map(String::as_str)
        .filter(|m| m.starts_with("PATCH /accounts/acct-1 k="))
        .collect();
    assert_eq!(
        k_patches,
        [
            "PATCH /accounts/acct-1 k=true",
            "PATCH /accounts/acct-1 k=false"
        ],
        "{:?}",
        state.mutations
    );
}

#[test]
fn an_empty_modes_flow_gets_no_paste() {
    let (report, stub) = safe_report(Plant::EmptyModes);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /clauth/v1/accounts/login",
            "modes: at least one of poll | paste",
            "[]"
        )]
    );
    assert!(
        !stub
            .state()
            .requests
            .iter()
            .any(|r| r.starts_with("POST /clauth/v1/accounts/login/f_")),
        "an empty `modes` array gets no paste: {:?}",
        stub.state().requests
    );
}

#[test]
fn a_non_poll_mode_gets_no_paste() {
    let (report, stub) = safe_report(Plant::PasteMode);

    assert_eq!(
        report.violations,
        vec![violation(
            "POST /clauth/v1/accounts/login",
            "modes[0]: one of poll | paste",
            "\"Paste\""
        )]
    );
    assert!(
        !stub
            .state()
            .requests
            .iter()
            .any(|r| r.starts_with("POST /clauth/v1/accounts/login/f_")),
        "a flow whose `modes` is not positively poll-only gets no paste: {:?}",
        stub.state().requests
    );
}

#[test]
fn an_oversized_health_body_is_named_as_no_answer() {
    let (report, _stub) = safe_report(Plant::BigHealthBody);

    assert_eq!(
        report.violations,
        vec![violation(
            "GET /health",
            "an answer",
            "no answer (the response body is larger than request limit: 1048576)"
        )]
    );
}

/// A record `id` of 129 astral characters (258 UTF-16 units) fails on both
/// reads that carry it, while a sibling at 128 characters (256 units) passes
/// clean: the checker counts the UTF-16 units the core counts, never chars.
#[test]
fn a_token_record_id_past_256_utf16_units_fails_though_its_chars_fit() {
    let stub = Stub::start(Plant::TokenIdPastUtf16Max);
    let (admin, key) = secrets();

    let report = check(&stub.base, &admin, &key, &Mode::Safe).expect("check runs");

    let at: Vec<(&str, &str)> = report
        .violations
        .iter()
        .map(|v| (v.route.as_str(), v.expected.as_str()))
        .collect();
    let want = "records[0].id: a string of 1 to 256 UTF-16 units";
    assert_eq!(
        at,
        vec![
            ("GET /clauth/v1/tokens", want),
            ("GET /clauth/v1/tokens?since={unknown}", want),
        ]
    );
}
