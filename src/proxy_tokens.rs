//! The tps producer's poll of every proxy serving token records (contract
//! 1.2, `GET /clauth/v1/tokens`): which proxies it asks, one cursor per proxy
//! for the producer's life, and the records each poll answers. What a record
//! does to a response lives in [`crate::tps`].
//!
//! A proxy is admitted to the poll while its registry row is enabled, its
//! admin token reads under the one admin-token rule, and its `/info` lists
//! `token_stats`, read on the refresh cadence, never per tick. Every request
//! carrying the admin token, each refresh's `/info` and each tick's poll alike,
//! first passes [`Control`](crate::proxy_account::Control)'s identity gate: the
//! bind's `/health` must name the proxy on a contract major clauth speaks. A
//! poll whose gate or read fails takes its proxy out of the poll and brings
//! the refresh forward to the next tick, at most once per cadence.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::proxy::{CONTROL, Service};
use crate::proxy_account::Secret;
use crate::proxy_check::WIRE_STRING_MAX;

/// One poll's or one `/info` read's whole bound, connect included. The
/// `/health` identity gate in front of each keeps the daemon probe's own
/// bounds.
const POLL_TIMEOUT: Duration = Duration::from_millis(800);

/// How often the registry, each proxy's identity and `/info`, and the
/// profiles holding proxy accounts are read again.
pub(crate) const REFRESH_INTERVAL_MS: u64 = 60_000;

/// A token-records body past this is not one. The widest conformant page, a
/// 1024-record ring whose `id` and `model` each fill 256 UTF-16 units, is
/// 3.4 MB once a six-byte escape encodes every unit; the 4 MiB cap holds it.
const BODY_LIMIT: u64 = 4 << 20;

/// One response a proxy measured, as the producer holds and saves it: the
/// client-visible message id, the proxy and account that served it, and what
/// the client saw. The account is resolved to a profile only when applied, so
/// a profile logged in later still takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenRecord {
    pub(crate) id: String,
    pub(crate) service: String,
    pub(crate) account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    pub(crate) output: u64,
    pub(crate) start_ms: u64,
    pub(crate) end_ms: u64,
}

/// Every claude profile holding a proxy account: service → account → profile.
pub(crate) type ProxyProfiles = HashMap<String, HashMap<String, String>>;

/// The proxy-account profiles in roster order; the first profile claiming an
/// account keeps it. An unreadable roster reads as none.
pub(crate) fn proxy_profiles() -> ProxyProfiles {
    let mut map = ProxyProfiles::new();
    let Ok(state) = crate::profile::load_app_state() else {
        return map;
    };
    for name in &state.profiles {
        if let Some(record) = crate::profile::stored_proxy_record(name) {
            map.entry(record.service.as_str().to_owned())
                .or_default()
                .entry(record.account)
                .or_insert_with(|| name.as_str().to_owned());
        }
    }
    map
}

/// The proxies the producer polls, and one cursor per proxy for its life.
#[derive(Debug, Default)]
pub(crate) struct Poller {
    proxies: BTreeMap<Service, Polled>,
    /// The cursor each proxy's last good answer served, kept while a refresh
    /// leaves the proxy out, so its next poll resumes rather than re-reading
    /// the whole ring.
    cursors: BTreeMap<Service, String>,
    /// When the last refresh ran; `None` before the first.
    refreshed_at: Option<u64>,
    /// When the last refresh a failed poll brought forward ran.
    early_refreshed_at: Option<u64>,
    /// A poll failed since the last refresh.
    poll_failed: bool,
}

#[derive(Debug)]
struct Polled {
    addr: SocketAddr,
    admin: Secret,
}

#[derive(Deserialize)]
struct Info {
    #[serde(default)]
    capabilities: Vec<Value>,
}

#[derive(Deserialize)]
struct Page {
    records: Vec<Value>,
    cursor: String,
}

#[derive(Deserialize)]
struct WireRecord {
    id: String,
    account: String,
    #[serde(default)]
    model: Option<String>,
    output_tokens: u64,
    started_at: String,
    ended_at: String,
}

impl Poller {
    /// Every record each polled proxy answers at `now_ms`, oldest first per
    /// proxy. A failed poll answers nothing for its proxy and keeps its cursor.
    pub(crate) fn poll(&mut self, now_ms: u64) -> Vec<TokenRecord> {
        let elapsed =
            |at: Option<u64>| at.is_none_or(|at| now_ms < at || now_ms - at >= REFRESH_INTERVAL_MS);
        let early = self.poll_failed && elapsed(self.early_refreshed_at);
        if early || elapsed(self.refreshed_at) {
            self.refresh();
            self.refreshed_at = Some(now_ms);
            if early {
                self.early_refreshed_at = Some(now_ms);
            }
            self.poll_failed = false;
        }
        if self.proxies.is_empty() {
            return Vec::new();
        }
        let agent = crate::proxy_account::control_agent(POLL_TIMEOUT);
        let mut records = Vec::new();
        let mut failed = Vec::new();
        for (service, proxy) in &self.proxies {
            let cursor = self.cursors.get(service);
            let path = match cursor {
                Some(cursor) => format!(
                    "/tokens?since={}",
                    crate::oauth_login::percent_encode(cursor)
                ),
                None => "/tokens".to_owned(),
            };
            // A bind that stopped answering as the proxy is sent nothing
            // carrying the admin token, and is handled as a failed poll.
            let page = answers_as(service, proxy.addr)
                .then(|| get::<Page>(&agent, proxy.addr, &proxy.admin, &path))
                .flatten()
                .filter(|page| !page.cursor.is_empty());
            let Some(page) = page else {
                failed.push(service.clone());
                continue;
            };
            self.cursors.insert(service.clone(), page.cursor);
            records.extend(
                page.records
                    .into_iter()
                    .filter_map(|record| into_record(service, record)),
            );
        }
        // A proxy whose poll failed waits for a refresh to admit it again, so
        // a wedged token route costs a tick at most once per refresh.
        for service in failed {
            self.proxies.remove(&service);
            self.poll_failed = true;
        }
        records
    }

    /// Re-read which proxies serve token records.
    fn refresh(&mut self) {
        let registry = crate::proxy::Registry::load().unwrap_or_default();
        let agent = crate::proxy_account::control_agent(POLL_TIMEOUT);
        let mut next = BTreeMap::new();
        for (service, row) in registry.iter().filter(|(_, row)| row.enabled) {
            let Some(admin) = crate::proxy::admin_token_path(service)
                .ok()
                .and_then(|path| crate::proxy_check::read_admin_token_optional(&path).ok())
                .flatten()
            else {
                continue;
            };
            let addr = crate::proxy::bind(row.port);
            if answers_as(service, addr) && lists_token_stats(&agent, addr, &admin) {
                next.insert(service.clone(), Polled { addr, admin });
            }
        }
        self.proxies = next;
    }
}

/// Whether `addr` answers `/health` as `service` on a contract major clauth
/// speaks: [`Control`](crate::proxy_account::Control)'s own identity gate,
/// asked before every request carrying the admin token.
fn answers_as(service: &Service, addr: SocketAddr) -> bool {
    crate::proxy_account::foreign_answer(service, addr).is_none()
}

fn lists_token_stats(agent: &ureq::Agent, addr: SocketAddr, admin: &Secret) -> bool {
    get::<Info>(agent, addr, admin, "/info").is_some_and(|info| {
        info.capabilities
            .iter()
            .any(|c| c.as_str() == Some("token_stats"))
    })
}

/// A control `GET` answering 200 with a body of `T`; `None` on anything else.
fn get<T: DeserializeOwned>(
    agent: &ureq::Agent,
    addr: SocketAddr,
    admin: &Secret,
    path: &str,
) -> Option<T> {
    let mut response = agent
        .get(&format!("http://{addr}{CONTROL}{path}"))
        .header("authorization", &format!("Bearer {}", admin.expose()))
        .call()
        .ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    let body = response
        .body_mut()
        .with_config()
        .limit(BODY_LIMIT)
        .read_to_string()
        .ok()?;
    serde_json::from_str(&body).ok()
}

/// One wire record as the producer holds it; `None` for a record off the
/// contract's shape, which the page's other records survive.
fn into_record(service: &Service, value: Value) -> Option<TokenRecord> {
    let wire: WireRecord = serde_json::from_value(value).ok()?;
    let wire_string = |s: &str| !s.is_empty() && s.encode_utf16().count() <= WIRE_STRING_MAX;
    if !wire_string(&wire.id) || wire.account.is_empty() {
        return None;
    }
    let start_ms = parse_ms(&wire.started_at)?;
    let end_ms = parse_ms(&wire.ended_at)?;
    if end_ms < start_ms {
        return None;
    }
    Some(TokenRecord {
        id: wire.id,
        service: service.as_str().to_owned(),
        account: wire.account,
        model: wire.model.filter(|m| wire_string(m)),
        output: wire.output_tokens,
        start_ms,
        end_ms,
    })
}

fn parse_ms(ts: &str) -> Option<u64> {
    let ms = chrono::DateTime::parse_from_rfc3339(ts)
        .ok()?
        .timestamp_millis();
    u64::try_from(ms).ok()
}

#[cfg(test)]
#[path = "../tests/inline/proxy_tokens.rs"]
mod tests;
