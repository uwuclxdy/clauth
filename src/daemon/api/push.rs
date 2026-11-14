//! Web Push registration: the daemon's VAPID key, one push subscription per
//! paired device, and the three routes a device manages its own row through.
//!
//! The key is an ECDSA P-256 keypair in `~/.clauth/push-vapid-key.der`
//! (PKCS#8), minted from the OS CSPRNG the first time anything needs it and
//! never minted again while that file exists: a new key orphans every
//! subscription a browser made against the old one. The subscriptions live in
//! `~/.clauth/push-subscriptions.json`, one row per device. Both files are
//! written under the state flock through the owner-only writer and reach the
//! disk before the answer, like the device list.
//!
//! What a row holds (the endpoint, the browser's `p256dh` and `auth` keys) is
//! exactly what a sender needs to put a readable push on that device, so none
//! of it leaves this module through a response, a log line, the status feed
//! or a `Debug`; the answers carry the event set and nothing else.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use ring::agreement;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use serde::{Deserialize, Serialize};

use super::devices::{Device, sync_to_disk};
use super::http::{Request, Response, flatten_control_chars, sanitize_for_log};
use super::routes::{ApiContext, Caller, ErrorBody};
use crate::lock::{StateLockHeld, StateLockTimeout, with_state_lock};
use crate::logline::logline;
use crate::profile::{atomic_write_600, clauth_dir};

const KEY_FILE: &str = "push-vapid-key.der";
const STORE_FILE: &str = "push-subscriptions.json";
/// Bumped only on a breaking change to the store's shape, like `devices.json`.
const SCHEMA: u64 = 1;

/// The push services an endpoint may name: Google's, Apple's and Mozilla's,
/// the three a browser hands a web app. Matched whole, never as a suffix, so
/// the daemon POSTs to nothing a device could steer it at.
pub(crate) const PUSH_HOSTS: [&str; 3] = [
    "fcm.googleapis.com",
    "web.push.apple.com",
    "updates.push.services.mozilla.com",
];

/// The reason an endpoint outside [`PUSH_HOSTS`] is refused with.
fn endpoint_refused() -> String {
    format!(
        "a push endpoint must be an https URL on the default port with no user part, whose host \
         is exactly one of {}",
        PUSH_HOSTS.join(", ")
    )
}

/// What a device can ask to be told about.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PushEvent {
    /// An agent is waiting on the user.
    Blocked,
    /// A login or key is failing.
    Account,
    /// A usage window crossed its line.
    Window,
    /// An account's usage-credit cap filled.
    Credit,
}

impl PushEvent {
    /// The set a device gets when it names none.
    const DEFAULT: [Self; 2] = [Self::Blocked, Self::Account];

    fn parse(name: &str) -> Option<Self> {
        match name {
            "blocked" => Some(Self::Blocked),
            "account" => Some(Self::Account),
            "window" => Some(Self::Window),
            "credit" => Some(Self::Credit),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Account => "account",
            Self::Window => "window",
            Self::Credit => "credit",
        }
    }
}

/// `events` in the order every answer lists them (`blocked`, `account`,
/// `window`, `credit`), each once.
fn canonical(mut events: Vec<PushEvent>) -> Vec<PushEvent> {
    events.sort_unstable();
    events.dedup();
    events
}

/// The daemon's VAPID signing key.
pub(crate) struct VapidKey {
    pair: EcdsaKeyPair,
}

impl std::fmt::Debug for VapidKey {
    /// Never the key: a formatter is how a key ends up in a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VapidKey(<redacted>)")
    }
}

impl VapidKey {
    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self> {
        EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &SystemRandom::new(),
        )
        .map(|pair| Self { pair })
        .map_err(|e| anyhow!("not a P-256 PKCS#8 key ({e})"))
    }

    /// The uncompressed public point (65 bytes, `0x04` first) as unpadded
    /// base64url: what a browser's `applicationServerKey` takes.
    pub(crate) fn public_key_b64(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.pair.public_key().as_ref())
    }

    /// An ES256 signature over `message`, as the fixed 64-byte `r || s` a
    /// JWT carries.
    pub(crate) fn sign(&self, message: &[u8]) -> Result<Vec<u8>> {
        self.pair
            .sign(&SystemRandom::new(), message)
            .map(|signature| signature.as_ref().to_vec())
            .map_err(|_| anyhow!("the VAPID signature failed"))
    }
}

fn key_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(KEY_FILE))
}

fn store_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(STORE_FILE))
}

/// The key on disk, `None` when there is no file yet. A file that does not
/// read or parse is an error, never an absent key: minting over it would
/// orphan every subscription made against it.
fn read_key(path: &Path) -> Result<Option<VapidKey>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    VapidKey::from_pkcs8(&bytes).map(Some).with_context(|| {
        format!(
            "{} is not a VAPID key clauth can read; clauth never mints a new key over it, so \
             move it aside to mint one, and every subscribed device then subscribes again",
            path.display()
        )
    })
}

/// The VAPID key, minted on first need. The common case reads the file
/// without the flock; only a missing file takes it, and the mint re-checks
/// under the hold so two first uses cannot mint two keys.
pub(crate) fn vapid_key() -> Result<VapidKey> {
    let path = key_path()?;
    if let Some(key) = read_key(&path)? {
        return Ok(key);
    }
    with_state_lock(|_held| {
        if let Some(key) = read_key(&path)? {
            return Ok(key);
        }
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .map_err(|_| anyhow!("CSPRNG failure minting the VAPID key"))?;
        atomic_write_600(&path, pkcs8.as_ref())
            .with_context(|| format!("failed to write {}", path.display()))?;
        sync_to_disk(&path)
            .with_context(|| format!("failed to flush {} to disk", path.display()))?;
        VapidKey::from_pkcs8(pkcs8.as_ref())
    })
}

/// One device's subscription: where its push service takes the push, the
/// browser's keys the payload is encrypted to, and the events it asked for.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Subscription {
    /// The device's name, as the device list holds it.
    pub(crate) device: String,
    /// [`super::devices::Device::pairing_id`] of the pairing that registered the row, so a
    /// device re-paired under the same name never inherits it.
    pairing: String,
    pub(crate) endpoint: String,
    /// The browser's P-256 public key, unpadded base64url.
    pub(crate) p256dh: String,
    /// The browser's 16-byte auth secret, unpadded base64url.
    pub(crate) auth: String,
    /// Event names as written; a name this build does not know is carried
    /// and never delivered.
    events: Vec<String>,
    /// Fields a newer clauth added, kept through this build's rewrites.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl std::fmt::Debug for Subscription {
    /// The device and its event set only: the endpoint and both keys are
    /// what it takes to push to that device.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("device", &self.device)
            .field("events", &self.events)
            .finish_non_exhaustive()
    }
}

impl Subscription {
    /// Whether this row asked for `event`.
    fn wants(&self, event: PushEvent) -> bool {
        self.events
            .iter()
            .any(|name| PushEvent::parse(name) == Some(event))
    }
}

/// `~/.clauth/push-subscriptions.json`.
#[derive(Serialize, Deserialize)]
pub(crate) struct Store {
    #[serde(default)]
    schema: u64,
    #[serde(default)]
    pub(crate) subscriptions: Vec<Subscription>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

impl Store {
    fn empty() -> Self {
        Self {
            schema: SCHEMA,
            subscriptions: Vec::new(),
            extra: serde_json::Map::new(),
        }
    }

    /// The index of `device`'s row. Device names are unique
    /// case-insensitively, so the lookup folds case like the device list's.
    fn index_of(&self, device: &str) -> Option<usize> {
        self.subscriptions
            .iter()
            .position(|row| row.device.eq_ignore_ascii_case(device))
    }
}

/// The store as it is on disk now. A missing file is an empty store; a file
/// that does not read or parse is an error, never an empty store, so no
/// writer lays a store over rows it could not read.
pub(crate) fn read_store() -> Result<Store> {
    let path = store_path()?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::empty()),
        Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut store: Store = serde_json::from_slice(&bytes).map_err(|e| {
        crate::format::parse_refusal(
            format!(
                "{} is not a push subscription list clauth can read",
                path.display()
            ),
            crate::format::json_parse_position(&e),
        )
    })?;
    store.schema = store.schema.max(SCHEMA);
    Ok(store)
}

/// Replace the store, and have it on disk before returning.
fn write_store(_held: &StateLockHeld, store: &Store) -> Result<()> {
    let path = store_path()?;
    let bytes =
        serde_json::to_vec_pretty(store).context("failed to encode the push subscription list")?;
    atomic_write_600(&path, bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    sync_to_disk(&path).with_context(|| format!("failed to flush {} to disk", path.display()))
}

/// The pairings the device list holds now, for a write to keep its rows to.
fn held_pairings() -> Result<HashSet<String>> {
    Ok(super::devices::read_store()?
        .devices
        .iter()
        .map(Device::pairing_id)
        .collect())
}

/// Drop `device`'s row under the caller's hold, and with it every row whose
/// pairing the device list no longer holds (one a failed revoke left). The
/// device revoke calls this inside its own flock section, after its own write;
/// a store with nothing to drop is left unwritten. `true` when `device` held a
/// row.
pub(crate) fn drop_device(held: &StateLockHeld, device: &str) -> Result<bool> {
    let mut store = read_store()?;
    let had_row = store.index_of(device).is_some();
    let pairings = held_pairings()?;
    let before = store.subscriptions.len();
    store
        .subscriptions
        .retain(|row| !row.device.eq_ignore_ascii_case(device) && pairings.contains(&row.pairing));
    if store.subscriptions.len() != before {
        write_store(held, &store)?;
    }
    Ok(had_row)
}

/// The rows a push for `event` goes to: each row that asked for it and whose
/// device the device list still holds under the pairing that registered it.
/// Read per push, like the device list per request, so a revoke or a drop
/// reaches the next push.
pub(crate) fn deliverable(event: PushEvent) -> Result<Vec<Subscription>> {
    let pairings = held_pairings()?;
    Ok(read_store()?
        .subscriptions
        .into_iter()
        .filter(|row| row.wants(event) && pairings.contains(&row.pairing))
        .collect())
}

/// What [`drop_if_endpoint`] found for the device.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dropped {
    /// Its row named the endpoint, and went.
    Row,
    /// Its row names another endpoint now, and stays.
    Replaced,
    /// It holds no row.
    NoRow,
}

/// Drop `device`'s row if it still names `endpoint`: a push service
/// answering 404 or 410 retires that endpoint, never one the device
/// registered since. Like every write, it drops the rows whose pairing the
/// device list no longer holds.
pub(crate) fn drop_if_endpoint(device: &str, endpoint: &str) -> Result<Dropped> {
    with_state_lock(|held| {
        let mut store = read_store()?;
        let found = match store.index_of(device) {
            None => Dropped::NoRow,
            Some(index) if store.subscriptions[index].endpoint == endpoint => Dropped::Row,
            Some(_) => Dropped::Replaced,
        };
        let gone = found == Dropped::Row;
        let pairings = held_pairings()?;
        let before = store.subscriptions.len();
        store.subscriptions.retain(|row| {
            !(gone && row.device.eq_ignore_ascii_case(device)) && pairings.contains(&row.pairing)
        });
        if store.subscriptions.len() != before {
            write_store(held, &store)?;
        }
        Ok(found)
    })
}

/// A revoke removed the device, but its push row could not be dropped. The
/// device is gone either way; the row stays behind inert, since no push goes
/// to a row whose pairing the device list no longer holds.
#[derive(Debug)]
pub(crate) struct SubscriptionLeft {
    pub(crate) device: String,
    pub(crate) cause: String,
    /// Whether the revoke that left this row behind also deleted the plaintext
    /// `auth_token.json` copy of the token despite the failed push drop: the
    /// caller says that deletion before reporting this leftover.
    pub(crate) legacy_deleted: bool,
}

impl SubscriptionLeft {
    /// The sentence every push-leftover message ends on — the lone one here and
    /// the combined one in `devices.rs` — sourced once so the two cannot drift:
    /// the row is inert, cleared by the next write, or now by moving the store
    /// aside.
    pub(crate) fn leftover_tail() -> String {
        format!(
            "it receives no push, since nothing is pushed to a pairing the device list no \
             longer holds, and the first subscription write after the list reads again drops \
             it; to clear it now, move ~/.clauth/{STORE_FILE} aside, and every subscribed \
             device then subscribes again"
        )
    }
}

impl std::fmt::Display for SubscriptionLeft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "revoked device '{}', but its push subscription could not be dropped ({}); {}",
            self.device,
            self.cause,
            Self::leftover_tail()
        )
    }
}

impl std::error::Error for SubscriptionLeft {}

/// The push service's origin (`https://<host>`) when `endpoint` may receive
/// a push: an https URL with no userinfo, on the default port, whose host is
/// exactly one of [`PUSH_HOSTS`]; `None` for anything else. Parsed by the
/// same `Uri` the outbound client sends through, so the host judged here is
/// the host a POST would reach, and the origin a VAPID token names is that
/// same host.
pub(crate) fn allowed_origin(endpoint: &str) -> Option<String> {
    let uri = endpoint.parse::<ureq::http::Uri>().ok()?;
    if !uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
    {
        return None;
    }
    let authority = uri.authority()?;
    // `Authority::host` keeps only what follows the LAST `@`, so a userinfo
    // part would otherwise be judged on the host it hides behind.
    if authority.as_str().contains('@') || !matches!(authority.port_u16(), None | Some(443)) {
        return None;
    }
    PUSH_HOSTS
        .iter()
        .find(|host| authority.host().eq_ignore_ascii_case(host))
        .map(|host| format!("https://{host}"))
}

/// Whether `bytes` is an uncompressed point on P-256, judged by the ECDH a
/// push runs against it: a point the sender could never encrypt to is refused
/// at registration rather than failing every push silently.
/// `Err` only for a CSPRNG failure minting the probe key, which is this
/// host's fault and never the client's.
fn is_p256_point(bytes: &[u8]) -> Result<bool> {
    if bytes.len() != 65 || bytes.first() != Some(&0x04) {
        return Ok(false);
    }
    let probe =
        agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &SystemRandom::new())
            .map_err(|_| anyhow!("CSPRNG failure minting the point-check key"))?;
    Ok(agreement::agree_ephemeral(
        probe,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, bytes),
        |_| (),
    )
    .is_ok())
}

/// Unpadded (or padded) base64url, decoded; `None` when it is neither.
pub(crate) fn decode_b64url(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .ok()
}

/// `GET /api/v1/push/key` — the VAPID public key.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PushKeyBody {
    /// The uncompressed P-256 point (65 bytes), unpadded base64url: the
    /// `applicationServerKey` a browser subscribes with.
    public_key: String,
}

/// The browser's keys for one subscription, as `PushSubscription.toJSON()`
/// carries them.
#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct PushKeys {
    /// The browser's P-256 public key: 65 bytes, base64url.
    p256dh: String,
    /// The browser's auth secret: 16 bytes, base64url.
    auth: String,
}

/// The fields `PUT /api/v1/push/subscription` accepts.
#[derive(Deserialize, utoipa::ToSchema)]
pub(crate) struct PushSubscriptionBody {
    /// The push service URL the browser handed out.
    endpoint: String,
    keys: PushKeys,
    /// The events to push; `blocked` and `account` when absent.
    #[serde(default)]
    events: Option<Vec<PushEvent>>,
}

/// The answer a registered subscription carries: the events it will push, in
/// canonical order.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PushSubscriptionOk {
    ok: bool,
    events: Vec<PushEvent>,
}

/// The answer a dropped subscription carries.
#[derive(Serialize, utoipa::ToSchema)]
pub(crate) struct PushSubscriptionDropped {
    ok: bool,
}

/// The answer a failed store or key operation gets: a held state flock is the
/// retryable 503, anything else the fixed 500, its chain in daemon.log.
fn failed(caller: &Caller<'_>, what: &str, e: &anyhow::Error) -> Response {
    logline!(
        "clauth api: device '{}' {what} failed: {}",
        caller.device_for_log(),
        sanitize_for_log(&format!("{e:#}"))
    );
    match e
        .chain()
        .find_map(|cause| cause.downcast_ref::<StateLockTimeout>())
    {
        Some(timeout) => Response::refused(
            503,
            "state_locked",
            &flatten_control_chars(&timeout.to_string()),
        ),
        None => Response::error(500, "internal"),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/push/key",
    responses(
        (status = 200, description = "the VAPID public key a browser subscribes with, minted on first need and stable after", body = PushKeyBody),
        (status = 401, description = "no bearer, or one matching no paired device (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 503, description = "the key is not minted yet and the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "the device list or the key does not read (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn key(_: &ApiContext, _: &Request, caller: &Caller<'_>) -> Response {
    match vapid_key() {
        Ok(key) => Response::serialize(
            200,
            &PushKeyBody {
                public_key: key.public_key_b64(),
            },
        ),
        Err(e) => failed(caller, "push key read", &e),
    }
}

/// `PUT /api/v1/push/subscription` — register or replace the calling device's
/// one subscription.
#[utoipa::path(
    put,
    path = "/api/v1/push/subscription",
    request_body = PushSubscriptionBody,
    responses(
        (status = 200, description = "the subscription is stored, replacing the device's earlier one; the events it pushes, in canonical order", body = PushSubscriptionOk),
        (status = 400, description = "the body is not a subscription, names an unknown event, or carries a p256dh that is not base64url of an uncompressed P-256 point or an auth that is not base64url of 16 bytes (`bad_request`), or the endpoint is not an https URL on the default port with no user part whose host is exactly fcm.googleapis.com, web.push.apple.com or updates.push.services.mozilla.com (`push_endpoint_not_allowed`)", body = ErrorBody),
        (status = 401, description = "no bearer, or one matching no paired device, the device's pairing included when a revoke lands while the subscription waits on the state flock (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "the device list or the subscription list does not read, the key check could not run, or the write failed (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn subscribe(_: &ApiContext, req: &Request, caller: &Caller<'_>) -> Response {
    // The router hands every bearer route its authenticated device.
    let Some(device) = caller.device else {
        return Response::error(500, "internal");
    };
    let Ok(body) = serde_json::from_slice::<PushSubscriptionBody>(&req.body) else {
        return Response::error(400, "bad_request");
    };
    if allowed_origin(&body.endpoint).is_none() {
        return Response::refused(400, "push_endpoint_not_allowed", &endpoint_refused());
    }
    let p256dh = decode_b64url(&body.keys.p256dh);
    let auth = decode_b64url(&body.keys.auth);
    let (Some(p256dh), Some(auth)) = (p256dh, auth) else {
        return Response::error(400, "bad_request");
    };
    match is_p256_point(&p256dh) {
        Ok(true) if auth.len() == 16 => {}
        Ok(_) => return Response::error(400, "bad_request"),
        Err(e) => return failed(caller, "push subscription key check", &e),
    }
    let events = canonical(body.events.unwrap_or_else(|| PushEvent::DEFAULT.to_vec()));
    let row = Subscription {
        device: device.name.clone(),
        pairing: device.pairing_id(),
        endpoint: body.endpoint,
        p256dh: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&p256dh),
        auth: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&auth),
        events: events.iter().map(|e| e.as_str().to_string()).collect(),
        extra: serde_json::Map::new(),
    };
    // The pairing is checked again under the hold: the router authenticated
    // off a flock-free read, and a revoke landing in between drops a row
    // that does not exist yet, so this write would outlive it.
    let stored = with_state_lock(|held| {
        let pairings = held_pairings()?;
        if !pairings.contains(&row.pairing) {
            return Ok(false);
        }
        let mut store = read_store()?;
        store.subscriptions.retain(|kept| {
            !kept.device.eq_ignore_ascii_case(&row.device) && pairings.contains(&kept.pairing)
        });
        store.subscriptions.push(row);
        write_store(held, &store).map(|()| true)
    });
    match stored {
        Ok(false) => Response::unauthorized(),
        Ok(true) => {
            let names: Vec<&str> = events.iter().map(|e| e.as_str()).collect();
            logline!(
                "clauth api: device '{}' registered a push subscription ({})",
                caller.device_for_log(),
                if names.is_empty() {
                    "no events".to_string()
                } else {
                    names.join(", ")
                }
            );
            Response::serialize(200, &PushSubscriptionOk { ok: true, events })
        }
        Err(e) => failed(caller, "push subscription write", &e),
    }
}

/// `DELETE /api/v1/push/subscription` — drop the calling device's
/// subscription.
#[utoipa::path(
    delete,
    path = "/api/v1/push/subscription",
    responses(
        (status = 200, description = "the device's subscription is dropped", body = PushSubscriptionDropped),
        (status = 401, description = "no bearer, or one matching no paired device, the device's pairing included when a revoke lands while the drop waits on the state flock (`unauthorized`)", body = ErrorBody),
        (status = 403, description = "a page's origin outside the app_origins list (`origin_not_allowed`); a device paired by a newer clauth with a tier this one does not know (`device_tier_unknown`)", body = ErrorBody),
        (status = 404, description = "the device holds no subscription (`no_push_subscription`)", body = ErrorBody),
        (status = 503, description = "the state flock is held (`state_locked`)", body = ErrorBody),
        (status = 500, description = "the device list or the subscription list does not read, or the write failed (`internal`)", body = ErrorBody)
    ),
    security(("bearer" = ["view"]))
)]
pub(crate) fn unsubscribe(_: &ApiContext, _: &Request, caller: &Caller<'_>) -> Response {
    // The router hands every bearer route its authenticated device.
    let Some(device) = caller.device else {
        return Response::error(500, "internal");
    };
    // The pairing is checked again under the hold, as the PUT's is: an old
    // pairing's DELETE that waited out a revoke and re-pair must not drop the
    // new pairing's row.
    let pairing = device.pairing_id();
    let dropped = with_state_lock(|held| {
        if !held_pairings()?.contains(&pairing) {
            return Ok(None);
        }
        drop_device(held, &device.name).map(Some)
    });
    match dropped {
        Ok(None) => Response::unauthorized(),
        Ok(Some(true)) => {
            logline!(
                "clauth api: device '{}' dropped its push subscription",
                caller.device_for_log()
            );
            Response::serialize(200, &PushSubscriptionDropped { ok: true })
        }
        Ok(Some(false)) => Response::error(404, "no_push_subscription"),
        Err(e) => failed(caller, "push subscription drop", &e),
    }
}

#[cfg(test)]
#[path = "../../../tests/inline/daemon_api_push_store.rs"]
mod tests;
