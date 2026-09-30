//! `clauth proxy check`: drive a running clauth proxy through contract v1 and
//! name every place it departs from the contract, as `route: expected X, got
//! Y` lines.
//!
//! The default run is safe on a live proxy: it reads every read route, checks
//! every `/info` declaration, probes each control route without the admin
//! token, hits each mutating route on an id no proxy holds (proving the route
//! exists and answers its error shape), starts one login flow and cancels it,
//! and sends one inference request. `--destructive <account>` adds the
//! mutating success paths on that account and ends by deleting it, for a
//! proxy's own CI against a stub upstream.
//!
//! Bodies are read as `serde_json::Value` rather than typed structs: the check
//! reports each wrong field by its path, which a failed typed deserialize
//! collapses into one error.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::proxy::{SPOKEN_MAJOR, contract_version};

const CONTROL: &str = "/clauth/v1";
/// An account id, flow id, action name and setting key no proxy holds: the
/// safe run's mutating requests aim here so they can only be refused.
const MISSING_ID: &str = "zz-check-nonexistent";
const UNKNOWN_SETTING: &str = "zz-check-unknown";
const NOT_A_KEY: &str = "clp_zz-check-not-a-key";
/// A model Claude Code itself sends, so any proxy that serves Claude Code
/// must accept it.
const INFERENCE_MODEL: &str = "claude-haiku-4-5";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const BODY_LIMIT: u64 = 1 << 20;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
/// An inference answer can wait on the provider (a queued off-peak channel).
const INFERENCE_TIMEOUT: Duration = Duration::from_secs(180);
/// How much of a quoted value a violation line keeps.
const REPR_MAX: usize = 80;
/// What a violation line keeps of the route and the expectation, both built
/// here but able to carry a proxy-supplied id.
const LINE_MAX: usize = 240;
/// The draft keeps a `/health` body under this many bytes, the daemon probe's
/// body cap.
const HEALTH_BODY_MAX: usize = 4096;
/// Longer than the draft's 15 s keepalive, so a stream whose head only rides
/// its first keepalive still answers inside it.
const SSE_HEAD_TIMEOUT: Duration = Duration::from_secs(20);

const KINDS: &[&str] = &["window", "balance", "channel", "offer", "stats"];
const UNITS: &[&str] = &["percent", "tokens", "requests", "currency", "custom"];
const ACCOUNT_STATES: &[&str] = &["ready", "login_required", "suspended"];
const STALE_REASONS: &[&str] = &["login_required", "upstream_unavailable", "rate_limited"];
const OFFER_STATES: &[&str] = &["available", "claiming", "claimed", "failed"];
const FLOW_STATES: &[&str] = &["pending", "done", "failed", "expired"];
const SETTING_TYPES: &[&str] = &["bool", "enum", "int", "number", "string"];
const CHAIN_ROLES: &[(&str, i64)] = &[("5h", 18_000), ("7d", 604_800)];

// ── inputs ──────────────────────────────────────────────────────────────────

/// A credential read from a file: no `Display`, and a `Debug` that never
/// prints the value.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    #[cfg(test)]
    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// The proxy's base URL: `http://` or `https://`, a host, and nothing after it
/// but an optional trailing slash, which is dropped.
pub(crate) fn base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim_end_matches('/');
    let host = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("https://"));
    match host {
        Some(host) if !host.is_empty() && !host.contains(['/', '?', '#', '@']) => {
            Ok(trimmed.to_string())
        }
        _ => Err(crate::usage_error(format!(
            "expected the proxy's base URL, like http://127.0.0.1:9101; got {raw:?}"
        ))),
    }
}

/// One credential alone in a file, never on argv. A trailing newline is
/// dropped; anything else that is not the credential is refused, as is a file
/// other users can read.
pub(crate) fn read_secret_file(path: &Path, what: &str) -> Result<Secret> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            return Err(crate::usage_error(format!(
                "cannot read {what} file {path:?} ({})",
                e.kind()
            )));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let open_to_others = std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o077 != 0)
            .unwrap_or(true);
        if open_to_others {
            return Err(crate::usage_error(format!(
                "{what} file {path:?} is accessible by other users; run `chmod 600` on it"
            )));
        }
    }
    let value = text.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        return Err(crate::usage_error(format!("{what} file {path:?} is empty")));
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(crate::usage_error(format!(
            "{what} file {path:?} holds whitespace or control characters"
        )));
    }
    Ok(Secret(value.to_string()))
}

// ── the report ──────────────────────────────────────────────────────────────

pub(crate) enum Mode {
    Safe,
    Destructive { account: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Violation {
    pub(crate) route: String,
    pub(crate) expected: String,
    pub(crate) got: String,
}

#[derive(Debug, Default)]
pub(crate) struct Report {
    pub(crate) checks: usize,
    pub(crate) violations: Vec<Violation>,
    /// Checks the run could not make on this proxy's data, each naming why.
    pub(crate) skipped: Vec<String>,
}

/// Each violation on its own line, then each skip, then the tally.
pub(crate) fn render(report: &Report) -> String {
    let mut out = String::new();
    for v in &report.violations {
        out.push_str(&format!(
            "{}: expected {}, got {}\n",
            v.route, v.expected, v.got
        ));
    }
    for skip in &report.skipped {
        out.push_str(&format!("skipped: {skip}\n"));
    }
    if report.violations.is_empty() {
        out.push_str(&format!("conformant: {} checks passed\n", report.checks));
    } else {
        out.push_str(&format!(
            "{} of {} checks failed\n",
            report.violations.len(),
            report.checks
        ));
    }
    out
}

/// The `clauth proxy check <target>` entry: the report on stdout, and exit 1
/// when it names any violation.
pub(crate) fn run(
    target: &str,
    admin_file: Option<&Path>,
    key_file: Option<&Path>,
    destructive: Option<String>,
) -> Result<()> {
    let (base, report) = check_target(target, admin_file, key_file, destructive)?;
    crate::out::out!("{}", render(&report));
    if report.violations.is_empty() {
        Ok(())
    } else {
        let n = report.violations.len();
        anyhow::bail!(
            "{base} departs from contract v1 in {n} place{}",
            if n == 1 { "" } else { "s" }
        )
    }
}

/// [`run`]'s check: the target resolved, and the base URL with its report.
fn check_target(
    target: &str,
    admin_file: Option<&Path>,
    key_file: Option<&Path>,
    destructive: Option<String>,
) -> Result<(String, Report)> {
    let Target {
        base,
        admin,
        key,
        service,
    } = resolve_target(target, admin_file, key_file, || {
        Ok(crate::profile::load_config()?.profiles)
    })?;
    if destructive.is_some() && key_file.is_none() {
        return Err(crate::usage_error(
            "--destructive needs --key-file holding the named account's key: a registered proxy's key is picked from a profile, which need not be that account's, and the proxy is your live one",
        ));
    }
    let mode = match destructive {
        Some(account) => Mode::Destructive { account },
        None => Mode::Safe,
    };
    let report = match &service {
        Some(service) => check_registered(&base, &admin, &key, &mode, service)?,
        None => check(&base, &admin, &key, &mode)?,
    };
    Ok((base, report))
}

/// What a check runs against: the proxy's base URL, the two credentials,
/// and for a registered proxy the service its `/health` must name.
pub(crate) struct Target {
    pub(crate) base: String,
    pub(crate) admin: Secret,
    pub(crate) key: Secret,
    pub(crate) service: Option<crate::proxy::Service>,
}

/// `clauth proxy check`'s positional and flags, resolved. A value with an
/// `http://` or `https://` scheme is the proxy's base URL and both files are
/// required; a proxy service names a registered proxy, whose bind, admin
/// token file and the one profile on its bind stand in for any flag not
/// given. `profiles` is read only when that profile is needed.
pub(crate) fn resolve_target(
    target: &str,
    admin_file: Option<&Path>,
    key_file: Option<&Path>,
    profiles: impl FnOnce() -> Result<Vec<crate::profile::Profile>>,
) -> Result<Target> {
    if target.starts_with("http://") || target.starts_with("https://") {
        let base = base_url(target)?;
        let admin_file = admin_file.ok_or_else(|| {
            crate::usage_error(
                "checking a proxy by URL needs --admin-token-file, the file holding its admin token",
            )
        })?;
        let key_file = key_file.ok_or_else(|| {
            crate::usage_error(
                "checking a proxy by URL needs --key-file, the file holding an inference key of one of its accounts",
            )
        })?;
        return Ok(Target {
            base,
            admin: read_secret_file(admin_file, "admin token")?,
            key: read_secret_file(key_file, "key")?,
            service: None,
        });
    }
    let Ok(service) = crate::proxy::Service::parse(target) else {
        return Err(crate::usage_error(format!(
            "expected a proxy's base URL, like http://127.0.0.1:9101, or the service of a proxy registered with `clauth proxy enable`, like zcode; got {target:?}"
        )));
    };
    let Some(row) = crate::proxy::Registry::load()?.get(&service).cloned() else {
        return Err(crate::proxy::not_registered(&service));
    };
    let admin = match admin_file {
        Some(path) => read_secret_file(path, "admin token")?,
        None => {
            let path = crate::proxy::admin_token_path(&service)?;
            if !path
                .try_exists()
                .with_context(|| format!("failed to inspect {}", path.display()))?
            {
                return Err(crate::usage_error(format!(
                    "proxy {:?} has no admin token file {path:?}; run `clauth proxy enable {service}` to mint it",
                    service.as_str()
                )));
            }
            read_secret_file(&path, "admin token")?
        }
    };
    let key = match key_file {
        Some(path) => read_secret_file(path, "key")?,
        None => key_on_bind(&profiles()?, &service, row.port)?,
    };
    Ok(Target {
        base: format!("http://{}", crate::proxy::bind(row.port)),
        admin,
        key,
        service: Some(service),
    })
}

/// The api key of the one profile whose `base_url` is the proxy's bind: a
/// proxy account is a profile pointed at it, and its key is the inference
/// key the check sends.
fn key_on_bind(
    profiles: &[crate::profile::Profile],
    service: &crate::proxy::Service,
    port: u16,
) -> Result<Secret> {
    let on_bind: Vec<&crate::profile::Profile> = profiles
        .iter()
        .filter(|profile| {
            profile
                .base_url
                .as_deref()
                .is_some_and(|url| is_loopback_url_on(url, port))
        })
        .collect();
    let bind = crate::proxy::bind(port);
    match on_bind.as_slice() {
        [] => Err(crate::usage_error(format!(
            "no profile's base_url is proxy {:?}'s bind http://{bind}; pass --key-file with an inference key of one of its accounts",
            service.as_str()
        ))),
        [profile] => match profile.api_key.as_deref().filter(|key| !key.is_empty()) {
            Some(key) => Ok(Secret(key.to_string())),
            None => Err(crate::usage_error(format!(
                "profile {:?} points at proxy {:?} and holds no api key; pass --key-file",
                profile.name.as_str(),
                service.as_str()
            ))),
        },
        several => {
            let names: Vec<String> = several
                .iter()
                .map(|profile| format!("{:?}", profile.name.as_str()))
                .collect();
            Err(crate::usage_error(format!(
                "profiles {} all point at proxy {:?} (http://{bind}); pass --key-file with the key to check with",
                names.join(", "),
                service.as_str()
            )))
        }
    }
}

/// Whether `url` is `http://` on a loopback host (`127.0.0.1`, `localhost`,
/// `[::1]`) and `port`, with nothing after but an optional trailing slash.
fn is_loopback_url_on(url: &str, port: u16) -> bool {
    let Some(rest) = url
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .and_then(|_| url.get(7..))
    else {
        return false;
    };
    let Some((host, url_port)) = rest.trim_end_matches('/').rsplit_once(':') else {
        return false;
    };
    ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .any(|loopback| host.eq_ignore_ascii_case(loopback))
        && url_port.bytes().all(|b| b.is_ascii_digit())
        && url_port.parse::<u16>() == Ok(port)
}

// ── the wire ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// Which credential a request carries, and in which header.
enum Auth<'a> {
    None,
    Bearer(&'a str),
    ApiKey(&'a str),
    ApiKeyAndBearer(&'a str, &'a str),
}

struct Answer {
    status: u16,
    content_type: String,
    www_authenticate: Option<String>,
    /// The first `Access-Control-Allow-*` header the answer carried: the
    /// contract serves no browser, so any one is a departure.
    cors: Option<String>,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .max_redirects_will_error(false)
        // The check asks the proxy directly: through an env-configured
        // forward proxy a loopback proxy resolves on the forward proxy's own
        // host, and any other one's delays and errors would be the forward
        // proxy's.
        .proxy(None)
        .build()
        .into()
}

fn send(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    auth: &Auth<'_>,
    body: Option<&str>,
    read_body: bool,
) -> Result<Answer, ureq::Error> {
    let mut headers: Vec<(&str, String)> =
        vec![("anthropic-version", ANTHROPIC_VERSION.to_string())];
    match auth {
        Auth::None => {}
        Auth::Bearer(token) => headers.push(("authorization", format!("Bearer {token}"))),
        Auth::ApiKey(key) => headers.push(("x-api-key", (*key).to_string())),
        Auth::ApiKeyAndBearer(key, bearer) => {
            headers.push(("x-api-key", (*key).to_string()));
            headers.push(("authorization", format!("Bearer {bearer}")));
        }
    }
    if body.is_some() {
        headers.push(("content-type", "application/json".to_string()));
    }
    macro_rules! with_headers {
        ($builder:expr) => {{
            let mut builder = $builder;
            for (name, value) in &headers {
                builder = builder.header(*name, value.as_str());
            }
            builder
        }};
    }
    let mut response = match method {
        Method::Get => with_headers!(agent.get(url)).call()?,
        Method::Delete => with_headers!(agent.delete(url)).call()?,
        Method::Post => with_headers!(agent.post(url)).send(body.unwrap_or(""))?,
        Method::Patch => with_headers!(agent.patch(url)).send(body.unwrap_or(""))?,
    };
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let content_type = header("content-type").unwrap_or_default();
    let www_authenticate = header("www-authenticate");
    let cors = response
        .headers()
        .keys()
        .map(|name| name.as_str())
        .find(|name| name.starts_with("access-control-allow-"))
        .map(str::to_string);
    let status = response.status().as_u16();
    let body = if read_body {
        response
            .body_mut()
            .with_config()
            .limit(BODY_LIMIT)
            .read_to_vec()?
    } else {
        Vec::new()
    };
    Ok(Answer {
        status,
        content_type,
        www_authenticate,
        cors,
        body,
    })
}

// ── field checks ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Kind {
    Str,
    Bool,
    Int,
    Num,
    Arr,
    Obj,
    Time,
    Id,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Str => "string",
            Self::Bool => "bool",
            Self::Int => "integer",
            Self::Num => "number",
            Self::Arr => "array",
            Self::Obj => "object",
            Self::Time => "RFC 3339 UTC time",
            Self::Id => "id ([a-z0-9][a-z0-9._-]{0,63})",
        }
    }

    fn admits(self, v: &Value) -> bool {
        match self {
            Self::Str => v.is_string(),
            Self::Bool => v.is_boolean(),
            Self::Int => v.is_i64() || v.is_u64(),
            Self::Num => v.is_number(),
            Self::Arr => v.is_array(),
            Self::Obj => v.is_object(),
            Self::Time => v.as_str().is_some_and(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .is_ok_and(|t| t.offset().local_minus_utc() == 0)
            }),
            Self::Id => v.as_str().is_some_and(is_id),
        }
    }
}

fn is_id(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 64
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A value as the report quotes it: its JSON text. [`Checker::violation`]
/// escapes and cuts it.
fn repr(v: &Value) -> String {
    v.to_string()
}

/// Proxy-supplied text made safe for a terminal: every control character (C0
/// and C1) and every bidi formatting character is written as its `\u{..}`
/// escape, so a proxy can neither drive the terminal nor reorder a line.
fn escape(text: &str) -> String {
    let bidi = |c: char| matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    text.chars()
        .map(|c| {
            if c.is_control() || bidi(c) {
                format!("\\u{{{:04x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

/// Two JSON values the contract treats as equal: numbers compare by value, so
/// a `4.0` sent reads back equal to the `4` a JSON encoder writes for it.
fn same(a: &Value, b: &Value) -> bool {
    a == b || (a.is_number() && b.is_number() && a.as_f64() == b.as_f64())
}

/// The flow a login answer opened, whatever its status: any 2xx whose body
/// names a `flow` that can sit in a path segment as-is, id form or not, so a
/// cancel reaches a flow a departing proxy opened too.
fn opened_flow(answer: &Answer) -> Option<String> {
    if !(200..300).contains(&answer.status) {
        return None;
    }
    answer
        .json()?
        .get("flow")?
        .as_str()
        .filter(|f| {
            // `.` and `..` are path-safe characters but not a segment.
            !matches!(*f, "." | "..")
                && (1..=256).contains(&f.len())
                && f.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~'))
        })
        .map(str::to_string)
}

/// `{key: value}`, for a key only known at run time.
fn one(key: &str, value: Value) -> Value {
    let mut map = serde_json::Map::new();
    map.insert(key.to_string(), value);
    Value::Object(map)
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}.{name}")
    }
}

// ── what the proxy declared ─────────────────────────────────────────────────

struct Health {
    service: Value,
    version: Value,
    contract: Value,
}

#[derive(Default)]
struct Declared {
    capabilities: Vec<String>,
    actions: Vec<ActionDecl>,
    figures: Vec<(String, String, Option<String>)>,
    settings: Vec<SettingDecl>,
}

struct ActionDecl {
    name: String,
    target: Option<String>,
}

struct SettingDecl {
    key: String,
    /// A `null` default: the setting may hold no value, so `null` is a valid
    /// current value.
    nullable: bool,
    account_scope: bool,
    typ: String,
    options: Vec<Value>,
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
    active_when: Option<(String, Vec<Value>)>,
}

impl SettingDecl {
    fn admits(&self, v: &Value) -> bool {
        let in_range = |n: f64| self.min.is_none_or(|m| n >= m) && self.max.is_none_or(|m| n <= m);
        match self.typ.as_str() {
            "bool" => v.is_boolean(),
            "enum" => self.options.contains(v),
            "int" => (v.is_i64() || v.is_u64()) && v.as_f64().is_some_and(in_range),
            "number" => v.as_f64().is_some_and(in_range),
            "string" => v.is_string(),
            _ => false,
        }
    }

    /// A valid value other than `current`, for the destructive run's
    /// set-and-restore; `None` where no such value exists (a free string).
    fn alternative(&self, current: &Value) -> Option<Value> {
        let candidates: Vec<Value> = match self.typ.as_str() {
            "bool" => vec![json!(!current.as_bool().unwrap_or(false))],
            "enum" => self.options.clone(),
            "int" | "number" => {
                let step = self.step.unwrap_or(1.0);
                let now = current.as_f64().unwrap_or(0.0);
                [self.min, self.max, Some(now + step), Some(now - step)]
                    .into_iter()
                    .flatten()
                    .map(|n| {
                        if self.typ == "int" {
                            json!(n as i64)
                        } else {
                            json!(n)
                        }
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        candidates
            .into_iter()
            .find(|v| !same(v, current) && self.admits(v))
    }

    /// A value of the wrong type for this setting, for the invalid probe.
    fn wrong_value(&self) -> Value {
        match self.typ.as_str() {
            "string" => json!(12_345),
            _ => json!("zz-check-invalid"),
        }
    }
}

struct AccountView {
    id: String,
    settings: serde_json::Map<String, Value>,
}

// ── the checker ─────────────────────────────────────────────────────────────

struct Checker<'a> {
    base: &'a str,
    /// The service a registered proxy's `/health` must name; `None` for a
    /// proxy checked by URL, which clauth holds no expectation of.
    expected_service: Option<&'a str>,
    admin: &'a Secret,
    key: &'a Secret,
    /// Every credential this run holds, the re-minted key included: redacted
    /// from every line the report prints, whatever a proxy echoes back.
    secrets: Vec<String>,
    control: ureq::Agent,
    inference: ureq::Agent,
    report: Report,
}

/// What `GET /accounts` answered: the accounts whose ids the check can put in
/// a path, and every string id it listed, well-formed or not.
#[derive(Default)]
struct Accounts {
    views: Vec<AccountView>,
    listed: Vec<String>,
}

/// Drive the proxy at `base` through contract v1. `Err` only when the check
/// cannot run at all (nothing listens) or `--destructive` names an account the
/// proxy does not list; every departure from the contract is a violation in
/// the report.
pub(crate) fn check(base: &str, admin: &Secret, key: &Secret, mode: &Mode) -> Result<Report> {
    check_expecting(base, admin, key, mode, None)
}

/// [`check`] on a registered proxy: a `/health` naming another service than
/// `service` stops the run there, before the admin token or the key is sent.
pub(crate) fn check_registered(
    base: &str,
    admin: &Secret,
    key: &Secret,
    mode: &Mode,
    service: &crate::proxy::Service,
) -> Result<Report> {
    check_expecting(base, admin, key, mode, Some(service.as_str()))
}

fn check_expecting(
    base: &str,
    admin: &Secret,
    key: &Secret,
    mode: &Mode,
    expected_service: Option<&str>,
) -> Result<Report> {
    let mut c = Checker {
        base,
        expected_service,
        admin,
        key,
        secrets: vec![admin.expose().to_string(), key.expose().to_string()],
        control: agent(CONTROL_TIMEOUT),
        inference: agent(INFERENCE_TIMEOUT),
        report: Report::default(),
    };
    let Some(health) = c.health()? else {
        return Ok(c.report);
    };
    c.control_auth();
    let declared = c.info(&health);
    let accounts = c.accounts(&declared);
    // Before any paid request: a mistyped account costs nothing.
    let target = match mode {
        Mode::Safe => None,
        Mode::Destructive { account } => c.destructive_target(account, &accounts)?,
    };
    let usage = c.usage(&declared, &accounts.views);
    c.config(&declared);
    c.missing_ids(&declared);
    c.login_flow(None);
    c.events(&declared);
    c.inference_routes(&declared);
    if let Some(view) = target {
        c.destructive(view, &declared, &usage);
    }
    Ok(c.report)
}

impl Checker<'_> {
    /// Every line the report prints passes here: credentials redacted first,
    /// then proxy-supplied text escaped, then cut.
    fn printable(&self, text: &str, max: usize) -> String {
        let redacted = self
            .secrets
            .iter()
            .filter(|s| !s.is_empty())
            .flat_map(|s| {
                // A secret quoted inside a JSON value reads JSON-escaped.
                let quoted = Value::String(s.clone()).to_string();
                [s.clone(), quoted[1..quoted.len() - 1].to_string()]
            })
            .fold(text.to_string(), |t, s| t.replace(&s, "<redacted>"));
        cut(&escape(&redacted), max)
    }

    fn violation(&mut self, route: &str, expected: String, got: String) {
        let violation = Violation {
            route: self.printable(route, LINE_MAX),
            expected: self.printable(&expected, LINE_MAX),
            got: self.printable(&got, REPR_MAX),
        };
        self.report.violations.push(violation);
    }

    fn skip(&mut self, what: String) {
        let line = self.printable(&what, LINE_MAX);
        self.report.skipped.push(line);
    }

    /// One request. A transport failure is itself a violation of `route`.
    fn call(
        &mut self,
        route: &str,
        method: Method,
        path: &str,
        auth: &Auth<'_>,
        body: Option<&Value>,
    ) -> Option<Answer> {
        let body = body.map(Value::to_string);
        self.call_raw(route, method, path, auth, body.as_deref(), true)
    }

    fn call_raw(
        &mut self,
        route: &str,
        method: Method,
        path: &str,
        auth: &Auth<'_>,
        body: Option<&str>,
        read_body: bool,
    ) -> Option<Answer> {
        let agent = if path.starts_with("/v1/") {
            &self.inference
        } else {
            &self.control
        };
        let url = format!("{}{path}", self.base);
        self.report.checks += 1;
        match send(agent, method, &url, auth, body, read_body) {
            Ok(answer) => {
                self.no_cors(route, &answer);
                Some(answer)
            }
            Err(e) => {
                self.violation(route, "an answer".to_string(), format!("no answer ({e})"));
                None
            }
        }
    }

    fn status(&mut self, route: &str, answer: &Answer, want: u16) -> bool {
        self.report.checks += 1;
        if answer.status == want {
            return true;
        }
        self.violation(
            route,
            format!("status {want}"),
            format!("status {}", answer.status),
        );
        false
    }

    fn no_cors(&mut self, route: &str, answer: &Answer) {
        self.report.checks += 1;
        if let Some(header) = &answer.cors {
            self.violation(
                route,
                "no Access-Control-Allow-* header".to_string(),
                header.clone(),
            );
        }
    }

    /// The answer's JSON body, served as `application/json`; a violation when
    /// it is either not JSON or labelled otherwise.
    fn body(&mut self, route: &str, answer: &Answer) -> Option<Value> {
        self.report.checks += 1;
        let parsed = answer.json();
        if parsed.is_none() {
            self.violation(
                route,
                "a JSON body".to_string(),
                format!("{} bytes that are not JSON", answer.body.len()),
            );
            return None;
        }
        self.report.checks += 1;
        if !answer
            .content_type
            .to_ascii_lowercase()
            .starts_with("application/json")
        {
            self.violation(
                route,
                "content-type: application/json".to_string(),
                repr(&json!(answer.content_type)),
            );
        }
        parsed
    }

    /// `route` answered `want` with a JSON body.
    fn expect_json(&mut self, route: &str, answer: Option<Answer>, want: u16) -> Option<Value> {
        let answer = answer?;
        if !self.status(route, &answer, want) {
            return None;
        }
        self.body(route, &answer)
    }

    /// A required field of `kind`.
    fn req<'v>(
        &mut self,
        route: &str,
        obj: &'v Value,
        path: &str,
        name: &str,
        kind: Kind,
    ) -> Option<&'v Value> {
        let at = join(path, name);
        self.report.checks += 1;
        match obj.get(name) {
            None => {
                self.violation(
                    route,
                    format!("{at}: {}", kind.name()),
                    "nothing".to_string(),
                );
                None
            }
            Some(v) if kind.admits(v) => Some(v),
            Some(v) => {
                // A string of the wrong form is worth quoting; any other
                // mismatch is a wrong type.
                let got = if v.is_string() && matches!(kind, Kind::Time | Kind::Id) {
                    repr(v)
                } else {
                    type_name(v).to_string()
                };
                self.violation(route, format!("{at}: {}", kind.name()), got);
                None
            }
        }
    }

    /// An optional field: absent or `null` passes, anything else must be of
    /// `kind`.
    fn opt<'v>(
        &mut self,
        route: &str,
        obj: &'v Value,
        path: &str,
        name: &str,
        kind: Kind,
    ) -> Option<&'v Value> {
        match obj.get(name) {
            None | Some(Value::Null) => None,
            Some(_) => self.req(route, obj, path, name, kind),
        }
    }

    fn one_of(&mut self, route: &str, v: &Value, at: &str, allowed: &[&str]) -> bool {
        self.report.checks += 1;
        if v.as_str().is_some_and(|s| allowed.contains(&s)) {
            return true;
        }
        self.violation(
            route,
            format!("{at}: one of {}", allowed.join(" | ")),
            repr(v),
        );
        false
    }

    fn req_one_of(
        &mut self,
        route: &str,
        obj: &Value,
        path: &str,
        name: &str,
        allowed: &[&str],
    ) -> Option<String> {
        let v = self.req(route, obj, path, name, Kind::Str)?;
        self.one_of(route, v, &join(path, name), allowed)
            .then(|| v.as_str().unwrap_or_default().to_string())
    }

    fn equals(&mut self, route: &str, got: Option<&Value>, at: &str, want: &Value) {
        self.report.checks += 1;
        if !got.is_some_and(|got| same(got, want)) {
            let shown = got.map_or_else(|| "nothing".to_string(), repr);
            self.violation(route, format!("{at}: {}", repr(want)), shown);
        }
    }

    fn items<'v>(
        &mut self,
        route: &str,
        obj: &'v Value,
        path: &str,
        name: &str,
    ) -> Vec<(String, &'v Value)> {
        let at = join(path, name);
        self.req(route, obj, path, name, Kind::Arr)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| (format!("{at}[{i}]"), item))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A control refusal: `status`, the daemon's error body naming `code`, and
    /// `field` when the refusal names one.
    fn control_error(
        &mut self,
        route: &str,
        answer: Option<Answer>,
        want: u16,
        code: &str,
        field: Option<&str>,
    ) {
        let Some(answer) = answer else { return };
        if !self.status(route, &answer, want) {
            return;
        }
        if want == 401 {
            self.bearer_challenge(route, &answer);
        }
        let Some(body) = self.body(route, &answer) else {
            return;
        };
        self.equals(route, body.get("ok"), "ok", &json!(false));
        self.equals(route, body.get("error"), "error", &json!(code));
        self.opt(route, &body, "", "reason", Kind::Str);
        match field {
            Some(field) => self.equals(route, body.get("field"), "field", &json!(field)),
            None => {
                self.opt(route, &body, "", "field", Kind::Str);
            }
        }
    }

    fn bearer_challenge(&mut self, route: &str, answer: &Answer) {
        self.report.checks += 1;
        if !answer
            .www_authenticate
            .as_deref()
            .is_some_and(|v| v.starts_with("Bearer"))
        {
            self.violation(
                route,
                "a WWW-Authenticate: Bearer header".to_string(),
                answer
                    .www_authenticate
                    .clone()
                    .unwrap_or_else(|| "none".to_string()),
            );
        }
    }

    /// An inference refusal: `status` and Anthropic's full error envelope.
    fn anthropic_error(&mut self, route: &str, answer: Option<Answer>, want: u16, kind: &str) {
        let Some(body) = self.expect_json(route, answer, want) else {
            return;
        };
        self.equals(route, body.get("type"), "type", &json!("error"));
        if let Some(error) = self.req(route, &body, "", "error", Kind::Obj) {
            self.equals(route, error.get("type"), "error.type", &json!(kind));
            self.req(route, error, "error", "message", Kind::Str);
        }
    }

    // ── legs ────────────────────────────────────────────────────────────────

    fn health(&mut self) -> Result<Option<Health>> {
        const ROUTE: &str = "GET /health";
        let url = format!("{}/health", self.base);
        self.report.checks += 1;
        // The daemon's own probe client: an answer it would time out on reads
        // as a wedged proxy there, so it is a departure here.
        let probe = crate::gateway::health_agent();
        let answer = match send(&probe, Method::Get, &url, &Auth::None, None, true) {
            Ok(answer) => answer,
            Err(ureq::Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                anyhow::bail!("nothing answers at {}; is the proxy running?", self.base)
            }
            Err(e) => {
                self.violation(
                    ROUTE,
                    format!(
                        "an answer inside the daemon probe's bounds ({} s to connect, {} s to answer)",
                        crate::gateway::HEALTH_CONNECT_SECS,
                        crate::gateway::HEALTH_RESPONSE_SECS
                    ),
                    format!("no answer ({e})"),
                );
                return Ok(None);
            }
        };
        self.report.checks += 1;
        if answer.body.len() >= HEALTH_BODY_MAX {
            self.violation(
                ROUTE,
                format!("a body under {HEALTH_BODY_MAX} bytes"),
                format!("{} bytes", answer.body.len()),
            );
        }
        self.no_cors(ROUTE, &answer);
        let Some(body) = self.expect_json(ROUTE, Some(answer), 200) else {
            return Ok(None);
        };
        self.equals(ROUTE, body.get("status"), "status", &json!("ok"));
        self.req(ROUTE, &body, "", "service", Kind::Id);
        if let Some(expected) = self.expected_service {
            self.report.checks += 1;
            if body.get("service") != Some(&json!(expected)) {
                self.violation(
                    ROUTE,
                    format!("service: {expected:?}, the registered proxy"),
                    body.get("service")
                        .map_or_else(|| "nothing".to_string(), repr),
                );
                return Ok(None);
            }
        }
        self.req(ROUTE, &body, "", "version", Kind::Str);
        let Some(contract) = self.req(ROUTE, &body, "", "contract", Kind::Str) else {
            return Ok(None);
        };
        let version = contract.as_str().and_then(contract_version);
        let major = version.map(|(major, _)| major);
        self.report.checks += 1;
        if major != Some(SPOKEN_MAJOR) {
            self.violation(
                ROUTE,
                format!("contract: major {SPOKEN_MAJOR}"),
                repr(contract),
            );
            return Ok(None);
        }
        let minor = version.map(|(_, minor)| minor);
        if minor.is_some_and(|m| m > 0) {
            self.skip(format!(
                "fields contract {} adds beyond 1.0: this clauth knows 1.0",
                contract.as_str().unwrap_or_default()
            ));
        }
        Ok(Some(Health {
            service: body["service"].clone(),
            version: body["version"].clone(),
            contract: contract.clone(),
        }))
    }

    /// Every control route refuses a request carrying no admin token; `/info`
    /// also refuses a wrong token and the inference key.
    fn control_auth(&mut self) {
        let missing = MISSING_ID;
        let routes: [(Method, &str, String, Option<Value>); 14] = [
            (Method::Get, "/info", "/info".into(), None),
            (Method::Get, "/accounts", "/accounts".into(), None),
            (
                Method::Post,
                "/accounts/login",
                "/accounts/login".into(),
                Some(json!({})),
            ),
            (
                Method::Get,
                "/accounts/login/{flow}",
                format!("/accounts/login/{missing}"),
                None,
            ),
            (
                Method::Post,
                "/accounts/login/{flow}",
                format!("/accounts/login/{missing}"),
                Some(json!({"code": "zz"})),
            ),
            (
                Method::Delete,
                "/accounts/login/{flow}",
                format!("/accounts/login/{missing}"),
                None,
            ),
            (
                Method::Patch,
                "/accounts/{id}",
                format!("/accounts/{missing}"),
                Some(json!({"settings": {}})),
            ),
            (
                Method::Post,
                "/accounts/{id}/key",
                format!("/accounts/{missing}/key"),
                None,
            ),
            (
                Method::Delete,
                "/accounts/{id}",
                format!("/accounts/{missing}"),
                None,
            ),
            (
                Method::Post,
                "/accounts/{id}/actions/{name}",
                format!("/accounts/{missing}/actions/{missing}"),
                Some(json!({})),
            ),
            (Method::Get, "/usage", "/usage".into(), None),
            (Method::Get, "/config", "/config".into(), None),
            (
                Method::Patch,
                "/config",
                "/config".into(),
                Some(json!({"values": {}})),
            ),
            (Method::Get, "/events", "/events".into(), None),
        ];
        for (method, template, path, body) in routes {
            let route = format!("{} {CONTROL}{template} (no token)", method.as_str());
            let path = format!("{CONTROL}{path}");
            let body = body.map(|b| b.to_string());
            // An event stream that wrongly answers without the token never
            // ends, so its body is left unread.
            let stream = template == "/events";
            let answer =
                self.call_raw(&route, method, &path, &Auth::None, body.as_deref(), !stream);
            if stream {
                if let Some(answer) = answer
                    && self.status(&route, &answer, 401)
                {
                    self.bearer_challenge(&route, &answer);
                }
                continue;
            }
            // A login the proxy wrongly started without the token is
            // cancelled, so the safe run leaves no flow polling upstream.
            let stray = answer.as_ref().and_then(opened_flow);
            self.control_error(&route, answer, 401, "unauthorized", None);
            self.cancel_stray(stray);
        }
        let info = format!("{CONTROL}/info");
        let key = self.key.expose().to_string();
        for (label, token) in [("wrong token", NOT_A_KEY), ("inference key", key.as_str())] {
            let route = format!("GET {info} ({label})");
            let answer = self.call(&route, Method::Get, &info, &Auth::Bearer(token), None);
            self.control_error(&route, answer, 401, "unauthorized", None);
        }
    }

    fn admin_call(
        &mut self,
        route: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Answer> {
        let admin = self.admin.expose().to_string();
        self.call(
            route,
            method,
            &format!("{CONTROL}{path}"),
            &Auth::Bearer(&admin),
            body,
        )
    }

    fn info(&mut self, health: &Health) -> Declared {
        let route = format!("GET {CONTROL}/info");
        let answer = self.admin_call(&route, Method::Get, "/info", None);
        let Some(body) = self.expect_json(&route, answer, 200) else {
            return Declared::default();
        };
        self.equals(&route, body.get("service"), "service", &health.service);
        self.equals(&route, body.get("version"), "version", &health.version);
        self.equals(&route, body.get("contract"), "contract", &health.contract);
        let mut declared = Declared::default();
        for (at, cap) in self.items(&route, &body, "", "capabilities") {
            self.report.checks += 1;
            match cap.as_str() {
                Some(cap) => declared.capabilities.push(cap.to_string()),
                None => self.violation(&route, format!("{at}: string"), type_name(cap).to_string()),
            }
        }
        for (at, action) in self.items(&route, &body, "", "actions") {
            let name = self
                .req(&route, action, &at, "name", Kind::Id)
                .and_then(Value::as_str)
                .map(str::to_string);
            self.req(&route, action, &at, "label", Kind::Str);
            self.req_one_of(&route, action, &at, "scope", &["account"]);
            let target = match action.get("target") {
                None | Some(Value::Null) => None,
                Some(t) => self
                    .one_of(&route, t, &join(&at, "target"), KINDS)
                    .then(|| t.as_str().unwrap_or_default().to_string()),
            };
            self.req(&route, action, &at, "confirm", Kind::Bool);
            if let Some(name) = name {
                declared.actions.push(ActionDecl { name, target });
            }
        }
        let mut ids = BTreeSet::new();
        for (at, figure) in self.items(&route, &body, "", "figures") {
            let id = self
                .req(&route, figure, &at, "id", Kind::Id)
                .and_then(Value::as_str)
                .map(str::to_string);
            let kind = self.req_one_of(&route, figure, &at, "kind", KINDS);
            self.req(&route, figure, &at, "label", Kind::Str);
            let chain = self.chain(&route, figure, &at, kind.as_deref());
            if let (Some(id), Some(kind)) = (id, kind) {
                self.unique(&route, &mut ids, &at, &id);
                declared.figures.push((id, kind, chain));
            }
        }
        let mut keys = BTreeSet::new();
        for (at, setting) in self.items(&route, &body, "", "settings") {
            if let Some(decl) = self.setting_decl(&route, setting, &at) {
                self.unique(&route, &mut keys, &at, &decl.key);
                declared.settings.push(decl);
            }
        }
        let known: BTreeSet<String> = declared.settings.iter().map(|s| s.key.clone()).collect();
        for (i, setting) in declared.settings.iter().enumerate() {
            if let Some((key, _)) = &setting.active_when {
                self.report.checks += 1;
                if !known.contains(key) {
                    self.violation(
                        &route,
                        format!("settings[{i}].active_when.key: a declared setting"),
                        repr(&json!(key)),
                    );
                }
            }
        }
        declared
    }

    fn unique(&mut self, route: &str, seen: &mut BTreeSet<String>, at: &str, id: &str) {
        self.report.checks += 1;
        if !seen.insert(id.to_string()) {
            self.violation(
                route,
                format!("{at}: an id no sibling repeats"),
                repr(&json!(id)),
            );
        }
    }

    /// A figure's chain role: only on a window, and only `5h` or `7d`.
    fn chain(
        &mut self,
        route: &str,
        figure: &Value,
        at: &str,
        kind: Option<&str>,
    ) -> Option<String> {
        let chain = match figure.get("chain") {
            None | Some(Value::Null) => return None,
            Some(chain) => chain,
        };
        let path = join(at, "chain");
        self.report.checks += 1;
        if kind.is_some_and(|k| k != "window") {
            self.violation(
                route,
                format!("{path}: only on a window figure"),
                repr(chain),
            );
            return None;
        }
        let roles: Vec<&str> = CHAIN_ROLES.iter().map(|(role, _)| *role).collect();
        self.one_of(route, chain, &path, &roles)
            .then(|| chain.as_str().unwrap_or_default().to_string())
    }

    fn setting_decl(&mut self, route: &str, s: &Value, at: &str) -> Option<SettingDecl> {
        let key = self
            .req(route, s, at, "key", Kind::Id)
            .and_then(Value::as_str)
            .map(str::to_string);
        let scope = self.req_one_of(route, s, at, "scope", &["proxy", "account"]);
        self.req(route, s, at, "label", Kind::Str);
        self.req(route, s, at, "hint", Kind::Str);
        let typ = self.req_one_of(route, s, at, "type", SETTING_TYPES);
        let numeric = matches!(typ.as_deref(), Some("int" | "number"));
        let bound = |c: &mut Self, name: &str| {
            let kind = if typ.as_deref() == Some("int") {
                Kind::Int
            } else {
                Kind::Num
            };
            if numeric {
                c.opt(route, s, at, name, kind).and_then(Value::as_f64)
            } else {
                c.absent(route, s, at, name, "only on a numeric setting");
                None
            }
        };
        let (min, max, step) = (bound(self, "min"), bound(self, "max"), bound(self, "step"));
        if numeric {
            self.opt(route, s, at, "unit", Kind::Str);
        } else {
            self.absent(route, s, at, "unit", "only on a numeric setting");
        }
        let mut options = Vec::new();
        if typ.as_deref() == Some("enum") {
            let items = self.items(route, s, at, "options");
            self.report.checks += 1;
            if items.is_empty() && s.get("options").is_some_and(Value::is_array) {
                self.violation(
                    route,
                    format!("{}: at least one option", join(at, "options")),
                    "[]".to_string(),
                );
            }
            for (opt_at, option) in items {
                if let Some(value) = self.req(route, option, &opt_at, "value", Kind::Str) {
                    options.push(value.clone());
                }
                self.req(route, option, &opt_at, "label", Kind::Str);
            }
        } else {
            self.absent(route, s, at, "options", "only on an enum setting");
        }
        let active_when = match s.get("active_when") {
            None | Some(Value::Null) => None,
            Some(_) => {
                let when = self.req(route, s, at, "active_when", Kind::Obj)?;
                let when_at = join(at, "active_when");
                let key = self
                    .req(route, when, &when_at, "key", Kind::Str)
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let values = self
                    .req(route, when, &when_at, "in", Kind::Arr)
                    .and_then(Value::as_array)
                    .cloned();
                key.zip(values)
            }
        };
        self.opt(route, s, at, "inactive_hint", Kind::Str);
        self.opt(route, s, at, "restart", Kind::Bool);
        let decl = SettingDecl {
            key: key?,
            nullable: s.get("default").is_some_and(Value::is_null),
            account_scope: scope? == "account",
            typ: typ?,
            options,
            min,
            max,
            step,
            active_when,
        };
        self.report.checks += 1;
        match s.get("default") {
            None => self.violation(
                route,
                format!("{}: the setting's type or null", join(at, "default")),
                "nothing".to_string(),
            ),
            Some(Value::Null) => {}
            Some(v) if decl.admits(v) => {}
            Some(v) => self.violation(
                route,
                format!("{}: a valid {} value", join(at, "default"), decl.typ),
                repr(v),
            ),
        }
        Some(decl)
    }

    fn absent(&mut self, route: &str, obj: &Value, at: &str, name: &str, why: &str) {
        self.report.checks += 1;
        if let Some(v) = obj.get(name).filter(|v| !v.is_null()) {
            self.violation(route, format!("{}: {why}", join(at, name)), repr(v));
        }
    }

    /// A settings object: every key declared at `scope`, every value valid.
    fn settings_values(
        &mut self,
        route: &str,
        values: &Value,
        at: &str,
        declared: &Declared,
        account_scope: bool,
    ) {
        let Some(values) = values.as_object() else {
            return;
        };
        for (key, value) in values {
            let path = join(at, key);
            self.report.checks += 1;
            match declared
                .settings
                .iter()
                .find(|s| s.key == *key && s.account_scope == account_scope)
            {
                None => {
                    let scope = if account_scope { "account" } else { "proxy" };
                    self.violation(
                        route,
                        format!("{path}: a declared {scope} setting"),
                        "undeclared".to_string(),
                    );
                }
                Some(decl) if !(decl.admits(value) || (value.is_null() && decl.nullable)) => {
                    self.violation(
                        route,
                        format!("{path}: a valid {} value", decl.typ),
                        repr(value),
                    );
                }
                Some(_) => {}
            }
        }
    }

    fn accounts(&mut self, declared: &Declared) -> Accounts {
        let route = format!("GET {CONTROL}/accounts");
        let answer = self.admin_call(&route, Method::Get, "/accounts", None);
        let Some(body) = self.expect_json(&route, answer, 200) else {
            return Accounts::default();
        };
        let mut accounts = Accounts::default();
        for (at, account) in self.items(&route, &body, "", "accounts") {
            if let Some(id) = account.get("id").and_then(Value::as_str) {
                accounts.listed.push(id.to_string());
            }
            if let Some(view) = self.account(&route, account, &at, declared) {
                accounts.views.push(view);
            }
        }
        accounts
    }

    /// The account `--destructive` may consume. A usage error when the proxy
    /// does not list it; `None`, with a skip line, when it lists it under an
    /// id no route can carry (that id is already a violation).
    fn destructive_target(
        &mut self,
        account: &str,
        accounts: &Accounts,
    ) -> Result<Option<AccountView>> {
        if let Some(view) = accounts.views.iter().find(|a| a.id == account) {
            return Ok(Some(AccountView {
                id: view.id.clone(),
                settings: view.settings.clone(),
            }));
        }
        if accounts.listed.iter().any(|id| id == account) {
            self.skip(format!(
                "--destructive: account {account:?} has an id outside the contract's id form, so no route can name it"
            ));
            return Ok(None);
        }
        let held = if accounts.listed.is_empty() {
            "none".to_string()
        } else {
            accounts
                .listed
                .iter()
                .map(|id| format!("{id:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        Err(crate::usage_error(self.printable(
            &format!("--destructive names account {account:?}, which this proxy does not list (it lists: {held})"),
            LINE_MAX,
        )))
    }

    fn account(
        &mut self,
        route: &str,
        account: &Value,
        at: &str,
        declared: &Declared,
    ) -> Option<AccountView> {
        let id = self
            .req(route, account, at, "id", Kind::Id)
            .and_then(Value::as_str)
            .map(str::to_string);
        self.req(route, account, at, "label", Kind::Str);
        self.req_one_of(route, account, at, "state", ACCOUNT_STATES);
        self.req(route, account, at, "created_at", Kind::Time);
        for (plan_at, plan) in self.items(route, account, at, "plans") {
            self.req(route, plan, &plan_at, "id", Kind::Id);
            self.req(route, plan, &plan_at, "label", Kind::Str);
        }
        let settings = self.req(route, account, at, "settings", Kind::Obj).cloned();
        if let Some(settings) = &settings {
            let settings_at = join(at, "settings");
            self.settings_values(route, settings, &settings_at, declared, true);
            // The card renders every account setting's current value, and the
            // destructive run restores from it.
            for decl in declared.settings.iter().filter(|s| s.account_scope) {
                self.report.checks += 1;
                if settings.get(&decl.key).is_none() {
                    self.violation(
                        route,
                        format!(
                            "{}: the setting's current value",
                            join(&settings_at, &decl.key)
                        ),
                        "nothing".to_string(),
                    );
                }
            }
        }
        Some(AccountView {
            id: id?,
            settings: settings
                .and_then(|s| s.as_object().cloned())
                .unwrap_or_default(),
        })
    }

    /// `GET /usage`, then the same filtered to the first account. Returns the
    /// unfiltered body for the destructive run's action targets.
    fn usage(&mut self, declared: &Declared, accounts: &[AccountView]) -> Value {
        let route = format!("GET {CONTROL}/usage");
        let answer = self.admin_call(&route, Method::Get, "/usage", None);
        let Some(body) = self.expect_json(&route, answer, 200) else {
            return Value::Null;
        };
        let ids: Vec<&str> = accounts.iter().map(|a| a.id.as_str()).collect();
        self.usage_body(&route, &body, declared, &ids);
        match accounts.first() {
            Some(first) => {
                let filtered = format!("GET {CONTROL}/usage?account={{id}}");
                let answer = self.admin_call(
                    &filtered,
                    Method::Get,
                    &format!("/usage?account={}", first.id),
                    None,
                );
                if let Some(one) = self.expect_json(&filtered, answer, 200) {
                    self.usage_body(&filtered, &one, declared, &[first.id.as_str()]);
                    let got: Vec<&Value> = one["accounts"]
                        .as_array()
                        .map(|a| a.iter().map(|u| &u["account"]).collect())
                        .unwrap_or_default();
                    self.report.checks += 1;
                    if got != [&json!(first.id)] {
                        self.violation(
                            &filtered,
                            format!("accounts: only {}", repr(&json!(first.id))),
                            repr(&json!(got)),
                        );
                    }
                }
            }
            None => self.skip(format!(
                "GET {CONTROL}/usage?account={{id}}: the proxy holds no account"
            )),
        }
        body
    }

    fn usage_body(&mut self, route: &str, body: &Value, declared: &Declared, ids: &[&str]) {
        for (at, account) in self.items(route, body, "", "accounts") {
            if let Some(id) = self.req(route, account, &at, "account", Kind::Str) {
                self.report.checks += 1;
                if !id.as_str().is_some_and(|id| ids.contains(&id)) {
                    self.violation(
                        route,
                        format!("{}: an account GET /accounts lists", join(&at, "account")),
                        repr(id),
                    );
                }
            }
            self.req_one_of(route, account, &at, "state", ACCOUNT_STATES);
            self.req(route, account, &at, "available", Kind::Bool);
            let mut roles = BTreeSet::new();
            let mut figure_ids = BTreeSet::new();
            for (fig_at, figure) in self.items(route, account, &at, "figures") {
                self.figure(
                    route,
                    figure,
                    &fig_at,
                    declared,
                    &mut roles,
                    &mut figure_ids,
                );
            }
        }
    }

    fn figure(
        &mut self,
        route: &str,
        figure: &Value,
        at: &str,
        declared: &Declared,
        roles: &mut BTreeSet<String>,
        ids: &mut BTreeSet<String>,
    ) {
        let kind = self.req_one_of(route, figure, at, "kind", KINDS);
        if let Some(id) = self
            .req(route, figure, at, "id", Kind::Id)
            .and_then(Value::as_str)
        {
            self.unique(route, ids, at, id);
            self.report.checks += 1;
            match declared.figures.iter().find(|(d, _, _)| d == id) {
                None => self.violation(
                    route,
                    format!("{}: a figure /info declares", join(at, "id")),
                    repr(&json!(id)),
                ),
                Some((_, declared_kind, declared_chain)) => {
                    if kind.as_deref().is_some_and(|k| k != declared_kind) {
                        self.violation(
                            route,
                            format!(
                                "{}: {:?}, as /info declares",
                                join(at, "kind"),
                                declared_kind
                            ),
                            repr(&figure["kind"]),
                        );
                    }
                    let got = figure.get("chain").cloned().unwrap_or(Value::Null);
                    let want = declared_chain.as_ref().map_or(Value::Null, |c| json!(c));
                    self.equals(route, Some(&got), &join(at, "chain"), &want);
                }
            }
        }
        self.req(route, figure, at, "label", Kind::Str);
        self.req(route, figure, at, "read_at", Kind::Time);
        if let Some(reason) = figure.get("stale_reason").filter(|v| !v.is_null()) {
            self.one_of(route, reason, &join(at, "stale_reason"), STALE_REASONS);
        }
        self.opt(route, figure, at, "summary", Kind::Str);
        match kind.as_deref() {
            Some("window") => self.window(route, figure, at, roles),
            Some("balance") => self.balance(route, figure, at),
            Some("channel") => {
                self.req(route, figure, at, "open", Kind::Bool);
                self.opt(route, figure, at, "next_open_at", Kind::Time);
                self.opt(route, figure, at, "queue_position", Kind::Int);
            }
            Some("offer") => self.offer(route, figure, at),
            Some("stats") => self.stats(route, figure, at),
            _ => {}
        }
        if kind.as_deref() != Some("window") {
            self.chain(route, figure, at, kind.as_deref());
        }
    }

    fn unit(&mut self, route: &str, obj: &Value, at: &str) -> Option<String> {
        let unit = self.req_one_of(route, obj, at, "unit", UNITS)?;
        if unit == "custom" {
            self.req(route, obj, at, "unit_label", Kind::Str);
        }
        Some(unit)
    }

    fn window(&mut self, route: &str, figure: &Value, at: &str, roles: &mut BTreeSet<String>) {
        self.req(route, figure, at, "used", Kind::Num);
        if let Some(limit) = self.req(route, figure, at, "limit", Kind::Num) {
            self.report.checks += 1;
            if limit.as_f64().is_none_or(|l| l <= 0.0) {
                self.violation(
                    route,
                    format!("{}: a number above 0", join(at, "limit")),
                    repr(limit),
                );
            }
        }
        self.unit(route, figure, at);
        let secs = self
            .req(route, figure, at, "window_secs", Kind::Int)
            .and_then(Value::as_i64);
        if figure.get("resets_at").is_none() {
            self.report.checks += 1;
            self.violation(
                route,
                format!("{}: RFC 3339 time or null", join(at, "resets_at")),
                "nothing".to_string(),
            );
        } else {
            self.opt(route, figure, at, "resets_at", Kind::Time);
        }
        let Some(role) = self.chain(route, figure, at, Some("window")) else {
            return;
        };
        if let Some((_, nominal)) = CHAIN_ROLES.iter().find(|(r, _)| *r == role) {
            self.report.checks += 1;
            if secs != Some(*nominal) {
                self.violation(
                    route,
                    format!("{}: {nominal} for chain {role:?}", join(at, "window_secs")),
                    secs.map_or_else(|| "nothing".to_string(), |s| s.to_string()),
                );
            }
        }
        self.unique(route, roles, &join(at, "chain"), &role);
    }

    fn balance(&mut self, route: &str, figure: &Value, at: &str) {
        self.req(route, figure, at, "remaining", Kind::Num);
        self.opt(route, figure, at, "limit", Kind::Num);
        self.opt(route, figure, at, "used", Kind::Num);
        if self.unit(route, figure, at).as_deref() == Some("currency") {
            self.req(route, figure, at, "currency", Kind::Str);
        }
        self.opt(route, figure, at, "expires_at", Kind::Time);
    }

    fn offer(&mut self, route: &str, figure: &Value, at: &str) {
        for (item_at, item) in self.items(route, figure, at, "items") {
            self.req(route, item, &item_at, "id", Kind::Id);
            self.req(route, item, &item_at, "label", Kind::Str);
            self.opt(route, item, &item_at, "description", Kind::Str);
            self.req_one_of(route, item, &item_at, "state", OFFER_STATES);
            self.opt(route, item, &item_at, "failure", Kind::Str);
            self.opt(route, item, &item_at, "starts_at", Kind::Time);
            self.opt(route, item, &item_at, "ends_at", Kind::Time);
            for (grant_at, grant) in self.items(route, item, &item_at, "grants") {
                self.req(route, grant, &grant_at, "label", Kind::Str);
                self.req(route, grant, &grant_at, "amount", Kind::Num);
                self.unit(route, grant, &grant_at);
                self.opt(route, grant, &grant_at, "effective_at", Kind::Time);
            }
        }
    }

    fn stats(&mut self, route: &str, figure: &Value, at: &str) {
        self.req(route, figure, at, "since", Kind::Time);
        if let Some(requests) = self.req(route, figure, at, "requests", Kind::Obj) {
            let req_at = join(at, "requests");
            for name in ["attempted", "succeeded", "failed", "cancelled"] {
                self.req(route, requests, &req_at, name, Kind::Int);
            }
        }
        if figure.get("mean_latency_ms").is_none() {
            self.report.checks += 1;
            self.violation(
                route,
                format!("{}: number or null", join(at, "mean_latency_ms")),
                "nothing".to_string(),
            );
        } else {
            self.opt(route, figure, at, "mean_latency_ms", Kind::Num);
        }
        if let Some(tokens) = self.req(route, figure, at, "tokens", Kind::Obj) {
            let tok_at = join(at, "tokens");
            for name in ["input", "output", "cache_read", "cache_creation"] {
                self.req(route, tokens, &tok_at, name, Kind::Int);
            }
        }
    }

    fn config(&mut self, declared: &Declared) {
        let route = format!("GET {CONTROL}/config");
        let answer = self.admin_call(&route, Method::Get, "/config", None);
        if let Some(body) = self.expect_json(&route, answer, 200)
            && let Some(values) = self.req(&route, &body, "", "values", Kind::Obj)
        {
            self.settings_values(&route, values, "values", declared, false);
        }
        let route = format!("PATCH {CONTROL}/config (unknown setting)");
        let answer = self.admin_call(
            &route,
            Method::Patch,
            "/config",
            Some(&json!({"values": one(UNKNOWN_SETTING, json!(true))})),
        );
        self.control_error(
            &route,
            answer,
            422,
            "unknown_setting",
            Some(UNKNOWN_SETTING),
        );
    }

    /// Each mutating route, aimed at ids no proxy holds: it must exist and
    /// refuse with its error shape, and nothing changes.
    fn missing_ids(&mut self, declared: &Declared) {
        let m = MISSING_ID;
        let action = declared
            .actions
            .first()
            .map_or(m, |a| a.name.as_str())
            .to_string();
        let routes: [(Method, &str, String, Option<Value>); 8] = [
            (
                Method::Patch,
                "/accounts/{id}",
                format!("/accounts/{m}"),
                Some(json!({"settings": {}})),
            ),
            (
                Method::Post,
                "/accounts/{id}/key",
                format!("/accounts/{m}/key"),
                None,
            ),
            (
                Method::Delete,
                "/accounts/{id}",
                format!("/accounts/{m}"),
                None,
            ),
            (
                Method::Post,
                "/accounts/{id}/actions/{name}",
                format!("/accounts/{m}/actions/{action}"),
                Some(json!({})),
            ),
            (
                Method::Get,
                "/accounts/login/{flow}",
                format!("/accounts/login/{m}"),
                None,
            ),
            (
                Method::Post,
                "/accounts/login/{flow}",
                format!("/accounts/login/{m}"),
                Some(json!({"code": "zz"})),
            ),
            (
                Method::Delete,
                "/accounts/login/{flow}",
                format!("/accounts/login/{m}"),
                None,
            ),
            (Method::Get, "/{unknown}", format!("/{m}"), None),
        ];
        let rebind = format!("POST {CONTROL}/accounts/login (re-bind, unknown id)");
        let answer = self.admin_call(
            &rebind,
            Method::Post,
            "/accounts/login",
            Some(&json!({"account": m})),
        );
        let stray = answer.as_ref().and_then(opened_flow);
        self.control_error(&rebind, answer, 404, "not_found", None);
        self.cancel_stray(stray);
        for (method, template, path, body) in routes {
            let route = format!("{} {CONTROL}{template} (unknown id)", method.as_str());
            let answer = self.admin_call(&route, method, &path, body.as_ref());
            self.control_error(&route, answer, 404, "not_found", None);
        }
    }

    /// Start one login flow (a re-bind of `account` when given), read it back,
    /// cancel it, and see it gone. A flow the proxy opened under any answer is
    /// cancelled, so no run leaves one polling upstream. The flow's
    /// `inference_key`, should one appear, is never quoted.
    fn login_flow(&mut self, account: Option<&str>) {
        let (route, request) = match account {
            None => (format!("POST {CONTROL}/accounts/login"), json!({})),
            Some(id) => (
                format!("POST {CONTROL}/accounts/login (re-bind)"),
                json!({"account": id}),
            ),
        };
        let Some(answer) = self.admin_call(&route, Method::Post, "/accounts/login", Some(&request))
        else {
            return;
        };
        let started = self.status(&route, &answer, 201);
        let body = if started {
            self.body(&route, &answer)
        } else {
            answer.json()
        };
        let flow = opened_flow(&answer);
        if started && let Some(body) = &body {
            self.flow_start(&route, body);
        }
        let Some(flow) = flow else { return };
        let path = format!("/accounts/login/{flow}");
        if started {
            let read = format!("GET {CONTROL}/accounts/login/{{flow}}");
            let answer = self.admin_call(&read, Method::Get, &path, None);
            if let Some(view) = self.expect_json(&read, answer, 200) {
                self.equals(&read, view.get("flow"), "flow", &json!(flow));
                if self
                    .req_one_of(&read, &view, "", "state", FLOW_STATES)
                    .as_deref()
                    == Some("pending")
                {
                    self.poll_hints(&read, &view);
                }
            }
        }
        let cancel = format!("DELETE {CONTROL}/accounts/login/{{flow}}");
        if let Some(answer) = self.admin_call(&cancel, Method::Delete, &path, None) {
            self.status(&cancel, &answer, 204);
        }
        if started {
            self.cancelled(&path);
        }
    }

    fn flow_start(&mut self, route: &str, body: &Value) {
        self.req(route, body, "", "flow", Kind::Id);
        self.equals(route, body.get("state"), "state", &json!("pending"));
        if let Some(url) = self.req(route, body, "", "url", Kind::Str) {
            self.report.checks += 1;
            if !url
                .as_str()
                .is_some_and(|u| u.starts_with("http://") || u.starts_with("https://"))
            {
                self.violation(route, "url: an http(s) URL".to_string(), repr(url));
            }
        }
        let modes = self.items(route, body, "", "modes");
        self.report.checks += 1;
        if modes.is_empty() && body.get("modes").is_some_and(Value::is_array) {
            self.violation(
                route,
                "modes: at least one of poll | paste".to_string(),
                "[]".to_string(),
            );
        }
        let mut seen = BTreeSet::new();
        for (at, mode) in modes {
            if self.one_of(route, mode, &at, &["poll", "paste"]) {
                self.unique(route, &mut seen, &at, mode.as_str().unwrap_or_default());
            }
        }
        self.poll_hints(route, body);
    }

    /// Cancel a flow a refusal should never have opened; the refusal's own
    /// status already names that departure.
    fn cancel_stray(&mut self, flow: Option<String>) {
        if let Some(flow) = flow {
            let cancel = format!("DELETE {CONTROL}/accounts/login/{{flow}} (stray flow)");
            let path = format!("/accounts/login/{flow}");
            self.admin_call(&cancel, Method::Delete, &path, None);
        }
    }

    /// A cancelled flow is gone: it reads back 404 `not_found`, like an
    /// unknown one.
    fn cancelled(&mut self, path: &str) {
        let gone = format!("GET {CONTROL}/accounts/login/{{flow}} (cancelled)");
        let answer = self.admin_call(&gone, Method::Get, path, None);
        self.control_error(&gone, answer, 404, "not_found", None);
    }

    fn poll_hints(&mut self, route: &str, body: &Value) {
        if let Some(interval) = self.req(route, body, "", "poll_interval_ms", Kind::Int) {
            self.report.checks += 1;
            if interval.as_u64().is_none_or(|ms| ms < 1000) {
                self.violation(
                    route,
                    "poll_interval_ms: at least 1000".to_string(),
                    repr(interval),
                );
            }
        }
        self.req(route, body, "", "expires_at", Kind::Time);
    }

    fn events(&mut self, declared: &Declared) {
        let route = format!("GET {CONTROL}/events");
        let admin = self.admin.expose().to_string();
        if declared.capabilities.iter().any(|c| c == "events") {
            let url = format!("{}{CONTROL}/events", self.base);
            self.report.checks += 1;
            let answer = match send(
                &agent(SSE_HEAD_TIMEOUT),
                Method::Get,
                &url,
                &Auth::Bearer(&admin),
                None,
                false,
            ) {
                Ok(answer) => answer,
                Err(e) => {
                    self.violation(
                        &route,
                        format!(
                            "a response head within {} s (an event stream sends its head before its first event)",
                            SSE_HEAD_TIMEOUT.as_secs()
                        ),
                        format!("no answer ({e})"),
                    );
                    return;
                }
            };
            self.no_cors(&route, &answer);
            if self.status(&route, &answer, 200) {
                self.report.checks += 1;
                if !answer
                    .content_type
                    .to_ascii_lowercase()
                    .starts_with("text/event-stream")
                {
                    self.violation(
                        &route,
                        "content-type: text/event-stream".to_string(),
                        repr(&json!(answer.content_type)),
                    );
                }
            }
        } else {
            let answer = self.admin_call(&route, Method::Get, "/events", None);
            self.control_error(&route, answer, 501, "unsupported", None);
        }
    }

    fn message_body() -> Value {
        json!({
            "model": INFERENCE_MODEL,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "Reply with the single word ok."}],
        })
    }

    fn message(&mut self, route: &str, auth: &Auth<'_>) {
        let answer = self.call(
            route,
            Method::Post,
            "/v1/messages",
            auth,
            Some(&Self::message_body()),
        );
        let Some(body) = self.expect_json(route, answer, 200) else {
            return;
        };
        self.equals(route, body.get("type"), "type", &json!("message"));
        self.equals(route, body.get("role"), "role", &json!("assistant"));
        self.req(route, &body, "", "content", Kind::Arr);
        self.req(route, &body, "", "model", Kind::Str);
        if let Some(usage) = self.req(route, &body, "", "usage", Kind::Obj) {
            self.req(route, usage, "usage", "input_tokens", Kind::Int);
            self.req(route, usage, "usage", "output_tokens", Kind::Int);
        }
    }

    fn refused_message(&mut self, route: &str, auth: &Auth<'_>) {
        let answer = self.call(
            route,
            Method::Post,
            "/v1/messages",
            auth,
            Some(&Self::message_body()),
        );
        self.anthropic_error(route, answer, 401, "authentication_error");
    }

    fn inference_routes(&mut self, declared: &Declared) {
        let key = self.key.expose().to_string();
        let admin = self.admin.expose().to_string();
        self.refused_message("POST /v1/messages (no key)", &Auth::None);
        self.refused_message("POST /v1/messages (admin token)", &Auth::ApiKey(&admin));
        self.refused_message("POST /v1/messages (unknown key)", &Auth::ApiKey(NOT_A_KEY));
        self.refused_message(
            "POST /v1/messages (x-api-key and bearer disagree)",
            &Auth::ApiKeyAndBearer(&key, NOT_A_KEY),
        );
        self.refused_message(
            "POST /v1/messages (unknown bearer key)",
            &Auth::Bearer(NOT_A_KEY),
        );
        self.message("POST /v1/messages", &Auth::ApiKey(&key));
        // A bearer key reaching body validation proves the header is accepted
        // without paying for a second answer.
        let route = "POST /v1/messages (bearer key, malformed body)";
        let answer = self.call_raw(
            route,
            Method::Post,
            "/v1/messages",
            &Auth::Bearer(&key),
            Some("{not json"),
            true,
        );
        self.anthropic_error(route, answer, 400, "invalid_request_error");
        let route = "POST /v1/messages/count_tokens";
        let body =
            json!({"model": INFERENCE_MODEL, "messages": [{"role": "user", "content": "ok"}]});
        let answer = self.call(
            route,
            Method::Post,
            "/v1/messages/count_tokens",
            &Auth::ApiKey(&key),
            Some(&body),
        );
        if declared.capabilities.iter().any(|c| c == "count_tokens") {
            if let Some(body) = self.expect_json(route, answer, 200) {
                self.req(route, &body, "", "input_tokens", Kind::Int);
            }
        } else {
            self.anthropic_error(route, answer, 404, "not_found_error");
        }
    }

    // ── destructive ─────────────────────────────────────────────────────────

    fn destructive(&mut self, view: AccountView, declared: &Declared, usage_body: &Value) {
        let account = view.id.as_str();
        let path = format!("/accounts/{account}");
        self.login_flow(Some(account));
        self.set_and_restore(&path, &view, declared);
        self.invalid_settings(&path, declared);
        self.actions(account, declared, usage_body);
        let fresh = self.remint(account);
        let delete = format!("DELETE {CONTROL}/accounts/{{id}}");
        if let Some(answer) = self.admin_call(&delete, Method::Delete, &path, None) {
            self.status(&delete, &answer, 204);
        }
        let listed = format!("GET {CONTROL}/accounts (after delete)");
        let answer = self.admin_call(&listed, Method::Get, "/accounts", None);
        if let Some(body) = self.expect_json(&listed, answer, 200) {
            self.report.checks += 1;
            let still = body["accounts"]
                .as_array()
                .is_some_and(|a| a.iter().any(|x| x["id"] == account));
            if still {
                self.violation(
                    &listed,
                    format!("accounts: without {account:?}"),
                    format!("{account:?} still listed"),
                );
            }
        }
        if let Some(fresh) = fresh {
            self.refused_message(
                "POST /v1/messages (re-minted key, account deleted)",
                &Auth::ApiKey(fresh.expose()),
            );
        }
    }

    /// Each active account setting: set another valid value, read it back,
    /// restore the original.
    fn set_and_restore(&mut self, path: &str, view: &AccountView, declared: &Declared) {
        let mut current = view.settings.clone();
        for decl in declared.settings.iter().filter(|s| s.account_scope) {
            // An absent value is already a violation of `GET /accounts`, and
            // there is nothing to restore it to.
            let Some(original) = current.get(&decl.key).cloned() else {
                self.skip(format!(
                    "setting {}: the account carries no current value",
                    decl.key
                ));
                continue;
            };
            if let Some((key, allowed)) = &decl.active_when
                && !allowed.contains(current.get(key).unwrap_or(&Value::Null))
            {
                self.skip(format!(
                    "setting {}: inactive under the account's current {key}",
                    decl.key
                ));
                continue;
            }
            let Some(alt) = decl.alternative(&original) else {
                self.skip(format!("setting {}: no other valid value to set", decl.key));
                continue;
            };
            for (value, label) in [(&alt, "set"), (&original, "restore")] {
                let route = format!("PATCH {CONTROL}/accounts/{{id}} ({label} {})", decl.key);
                let answer = self.admin_call(
                    &route,
                    Method::Patch,
                    path,
                    Some(&json!({"settings": one(&decl.key, value.clone())})),
                );
                if let Some(body) = self.expect_json(&route, answer, 200) {
                    self.equals(
                        &route,
                        body.get("settings").and_then(|s| s.get(&decl.key)),
                        &format!("settings.{}", decl.key),
                        value,
                    );
                }
            }
            current.insert(decl.key.clone(), original);
        }
    }

    /// A wrong-typed value and an unknown key are both refused, naming the
    /// field.
    fn invalid_settings(&mut self, path: &str, declared: &Declared) {
        if let Some(decl) = declared.settings.iter().find(|s| s.account_scope) {
            let route = format!("PATCH {CONTROL}/accounts/{{id}} (invalid {})", decl.key);
            let answer = self.admin_call(
                &route,
                Method::Patch,
                path,
                Some(&json!({"settings": one(&decl.key, decl.wrong_value())})),
            );
            self.control_error(&route, answer, 422, "invalid_setting", Some(&decl.key));
        }
        let route = format!("PATCH {CONTROL}/accounts/{{id}} (unknown setting)");
        let answer = self.admin_call(
            &route,
            Method::Patch,
            path,
            Some(&json!({"settings": one(UNKNOWN_SETTING, json!(true))})),
        );
        self.control_error(
            &route,
            answer,
            422,
            "unknown_setting",
            Some(UNKNOWN_SETTING),
        );
    }

    fn actions(&mut self, account: &str, declared: &Declared, usage: &Value) {
        for action in &declared.actions {
            let body = match &action.target {
                None => json!({}),
                Some(kind) => {
                    let item = usage["accounts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|u| u["account"] == account)
                        .flat_map(|u| u["figures"].as_array().into_iter().flatten())
                        .filter(|f| f["kind"] == kind.as_str())
                        .flat_map(|f| f["items"].as_array().into_iter().flatten())
                        .find_map(|i| i["id"].as_str());
                    let Some(item) = item else {
                        self.skip(format!("action {}: no {kind} item to target", action.name));
                        continue;
                    };
                    json!({"target": item})
                }
            };
            let route = format!("POST {CONTROL}/accounts/{{id}}/actions/{}", action.name);
            let answer = self.admin_call(
                &route,
                Method::Post,
                &format!("/accounts/{account}/actions/{}", action.name),
                Some(&body),
            );
            if let Some(body) = self.expect_json(&route, answer, 202) {
                self.equals(&route, body.get("accepted"), "accepted", &json!(true));
            }
        }
    }

    /// Re-mint the key: the old one stops answering, the new one answers.
    /// Neither is ever quoted.
    fn remint(&mut self, account: &str) -> Option<Secret> {
        let route = format!("POST {CONTROL}/accounts/{{id}}/key");
        let answer = self.admin_call(
            &route,
            Method::Post,
            &format!("/accounts/{account}/key"),
            None,
        );
        let body = self.expect_json(&route, answer, 200)?;
        let fresh = self
            .req(&route, &body, "", "inference_key", Kind::Str)?
            .as_str()?
            .to_string();
        self.report.checks += 1;
        if fresh.chars().any(|c| c.is_control() || c.is_whitespace()) {
            self.violation(
                &route,
                "inference_key: no whitespace or control characters".to_string(),
                "a key holding them".to_string(),
            );
            return None;
        }
        self.secrets.push(fresh.clone());
        let old = self.key.expose().to_string();
        // Also the line a key file holding another account's key produces.
        self.refused_message(
            "POST /v1/messages (the key file's key after the re-mint)",
            &Auth::ApiKey(&old),
        );
        self.message("POST /v1/messages (re-minted key)", &Auth::ApiKey(&fresh));
        Some(Secret(fresh))
    }
}

#[cfg(test)]
#[path = "../tests/inline/proxy_check.rs"]
mod tests;
