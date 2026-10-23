//! Stored identity per half and the two-layer forget a change site runs.
//!
//! Identity is always read from the stored files, never an in-memory `Profile`;
//! the changed answer is spelled once per half. The memory clears rank BELOW
//! `Config`, so they run after the config guard drops; the per-process epoch
//! gates the startup seeds.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

use crate::lockorder::{RankedMutex, rank};
use crate::profile::{
    AccountId, ConsoleCredential, ProfileName, StoredEndpointIdentity, stored_access_token,
    stored_endpoint_identity,
};
use crate::profile_cache::{
    ACCOUNT_ID_CACHE_FILE, KICK_BLOCK_CACHE_FILE, PROFILE_FETCHED_CACHE_FILE,
    THIRD_PARTY_CACHE_FILE, USAGE_CACHE_FILE, load_profile_cache, remove_profile_cache,
};
use crate::providers::Provider;

use super::scheduler::{
    KickBlocks, LastFetchedAt, NextRefreshPerProfile, PollStreaks, StatusStore,
    SuppressedAuthExpiredStore, ThirdPartyBroken, ThirdPartyStatusStore, ThirdPartyStreaks,
    ThirdPartyUsageStore, UsageStore, WeeklyResetKicks, forget_codex_polled, seed_codex_name,
    seed_third_party_name, third_party_credentialed_fields, try_seed_cache,
};
use super::{AutoStartQueueState, FetchLeg, forget_profile_memo, now_ms};

/// One half of a profile's credential identity. `Throughput` has no memory
/// store — `forget_memory` skips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Half {
    OAuth,
    ThirdParty,
    Codex,
    Throughput,
}

/// The halves a change site decided to forget.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForgetPlan {
    pub(crate) oauth: bool,
    pub(crate) third_party: bool,
    pub(crate) codex: bool,
    pub(crate) throughput: bool,
}

impl ForgetPlan {
    /// The flagged halves, in a fixed order.
    fn changed_halves(self) -> impl Iterator<Item = Half> {
        [Half::OAuth, Half::ThirdParty, Half::Codex, Half::Throughput]
            .into_iter()
            .filter(move |h| match h {
                Half::OAuth => self.oauth,
                Half::ThirdParty => self.third_party,
                Half::Codex => self.codex,
                Half::Throughput => self.throughput,
            })
    }
}

/// SHA-256 over length-delimited fields, head-truncated to a u64 — the same
/// shape as the third-party fingerprint, so no two field splits collide. A
/// collision costs one extra or one missed forget, never a wrong identity on
/// disk.
fn digest_fields(fields: &[&[u8]]) -> u64 {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    for field in fields {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    let digest = hasher.finalize();
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(head)
}

/// SHA-256 of the access token alone — the only stored credential a `/profile`
/// read is evidence about.
fn stored_token_digest(access_token: &str) -> u64 {
    digest_fields(&[access_token.as_bytes()])
}

/// The OAuth half's identity: the anchor plus the stored-token digest, and on a
/// profile with no pair stored its setup token's digest. The stored login is
/// the dominant fact — a stale anchor file left behind by a log out reads
/// `Unanchored(None)`, never an anchor without a login.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OAuthIdentity {
    pub(crate) anchor: Option<AccountId>,
    pub(crate) stored: Option<u64>,
    /// The digest of the token in `session-token.json`, set only while no pair
    /// is stored: that is the token the usage probe reads the account with, so
    /// a re-mint forgets the old readings the way a re-login does.
    /// Its own field rather than `stored`, which the throughput identity reads
    /// and which keys on the pair.
    pub(crate) setup_token: Option<u64>,
}

impl OAuthIdentity {
    /// The identity of the values about to be written: the anchor the site
    /// leaves on disk and the new credentials file's token.
    pub(crate) fn for_values(anchor: Option<&AccountId>, stored_token: Option<&str>) -> Self {
        match stored_token {
            Some(token) => Self {
                anchor: anchor.filter(|u| !u.trim().is_empty()).cloned(),
                stored: Some(stored_token_digest(token)),
                setup_token: None,
            },
            None => Self::default(),
        }
    }

    /// The identity of a profile with no pair stored whose setup token is
    /// `token`.
    pub(crate) fn for_setup_token(token: &str) -> Self {
        Self {
            setup_token: Some(stored_token_digest(token)),
            ..Self::default()
        }
    }
}

/// The digest of the setup token a switch would install from `name`'s
/// `session-token.json` ([`crate::claude::installed_session_token`]'s rule),
/// plus its unreadable flag: a sidecar that exists but does not parse is
/// unreadable, never "no setup token".
fn read_setup_token_for_compare(name: &ProfileName) -> (Option<u64>, bool) {
    if let Some(token) = crate::claude::installed_session_token(name) {
        return (Some(stored_token_digest(&token)), false);
    }
    let torn = crate::profile::profile_subpath(name, "session-token.json")
        .is_ok_and(|path| path.exists())
        && crate::claude::session_token_status(name).is_none();
    (None, torn)
}

/// The profile's stored claude OAuth access token, following the loader's
/// staged-pair rule: a live `credentials.json.staged` pair is the chain head,
/// so it is the stored token even while memory and the store lag it. `None` on
/// an absent/unparseable store or a login-less chain.
fn stored_token_with_stage(name: &ProfileName) -> Option<String> {
    let raw = crate::oauth::read_raw_store(name).ok().flatten();
    crate::oauth::live_stage_credentials(name, raw.as_ref())
        .and_then(|c| c.access_token().map(str::to_string))
        .or_else(|| raw.and_then(|c| c.access_token().map(str::to_string)))
}

/// The OAuth identity a spawn captures for the in-memory token `access_token`.
///
/// The disk anchor is adopted ONLY when the stored token (read with the
/// loader's staged-pair rule) still equals the in-memory token; otherwise the
/// unanchored in-memory digest is carried, which compares as changed against
/// any other stored token and discards the landing. The anchor is read BEFORE
/// the stored-token read, so a peer's install landing between the two reads
/// shows up as a token change — never a stale anchor paired with a newer token
/// (which `oauth_changed`'s anchored arm would let through).
pub(crate) fn spawn_oauth_identity(name: &ProfileName, access_token: &str) -> OAuthIdentity {
    let anchor = load_profile_cache::<AccountId>(name, ACCOUNT_ID_CACHE_FILE);
    let adopt_anchor = stored_token_with_stage(name).as_deref() == Some(access_token);
    OAuthIdentity::for_values(
        adopt_anchor.then_some(anchor).flatten().as_ref(),
        Some(access_token),
    )
}

/// Read `name`'s stored OAuth identity from the raw files (`account_id.json`,
/// `credentials.json`, and with no pair stored `session-token.json`).
pub(crate) fn read_oauth_identity(name: &ProfileName) -> OAuthIdentity {
    let stored = stored_access_token(name).map(|t| stored_token_digest(&t));
    let Some(stored) = stored else {
        return pairless_identity(name);
    };
    let anchor = load_profile_cache::<AccountId>(name, ACCOUNT_ID_CACHE_FILE)
        .filter(|u| !u.trim().is_empty());
    OAuthIdentity {
        anchor,
        stored: Some(stored),
        setup_token: None,
    }
}

/// The OAuth identity `name` has with no pair stored: its setup token's, off
/// `session-token.json` as it stands. What a site that drops the pair and
/// leaves the sidecar in place compares against.
pub(crate) fn pairless_identity(name: &ProfileName) -> OAuthIdentity {
    OAuthIdentity {
        setup_token: read_setup_token_for_compare(name).0,
        ..OAuthIdentity::default()
    }
}

/// The forget a `session-token.json` writer runs. With no pair stored the
/// setup token is the OAuth half's identity, so writing `after_token` (or
/// removing the sidecar, `None`) changes the half when its digest moves;
/// beside a pair the sidecar is no part of it. Read inside the writer's
/// state-flock hold, before the write.
pub(crate) fn setup_token_plan(name: &ProfileName, after_token: Option<&str>) -> ForgetPlan {
    let before = read_oauth_identity(name);
    if before.stored.is_some() {
        return ForgetPlan::default();
    }
    let after = after_token.map_or_else(OAuthIdentity::default, OAuthIdentity::for_setup_token);
    ForgetPlan {
        oauth: oauth_changed(&before, &after),
        ..ForgetPlan::default()
    }
}

/// The OAuth changed predicate, applied to two stored identities.
///
/// Changed: the login appears or disappears; the anchor moves A→B; the anchor
/// is removed while a login stays (an Unproven install); an unanchored
/// profile's stored token changes; with no pair on either side, the setup
/// token changes, appears or goes. NOT changed: an anchor appearing over an
/// unchanged stored token (a backfill, an adopt, a proven install's seed), nor
/// a rotation on an anchored profile.
pub(crate) fn oauth_changed(before: &OAuthIdentity, after: &OAuthIdentity) -> bool {
    // A setup token only ever stands beside no pair, so never beside an
    // anchor: the two anchored-before arms cannot meet one.
    match (before.anchor.as_ref(), after.anchor.as_ref()) {
        (Some(a), Some(b)) => a != b,
        (Some(_), None) => true,
        (None, _) => before.stored != after.stored || before.setup_token != after.setup_token,
    }
}

/// The provider literal the digest uses, a literal per variant rather than the
/// user-facing display name (which may be reworded and would reset identities).
fn provider_bytes(provider: Provider) -> &'static [u8] {
    match provider {
        Provider::DeepSeek => b"deepseek",
        Provider::Zai => b"zai",
        Provider::Alibaba => b"alibaba",
        Provider::OpenRouter => b"openrouter",
        Provider::MiniMax => b"minimax",
    }
}

/// The third-party usage identity as a digest: the provider plus what its fetch
/// sends. Alibaba usage reads the console session alone (key-less console
/// profiles included, an inference-key edit keeps the console reading); a typed
/// key provider sends the api key; a generic provider sends the api key AND the
/// resolved base_url, so a key edit over the same base_url is a change. `None`
/// when the profile has no third-party credential at all (the same credential
/// test the fetch leg applies).
pub(crate) fn third_party_identity(
    provider: Option<Provider>,
    api_key: Option<&str>,
    console: Option<&ConsoleCredential>,
    base_url: Option<&str>,
) -> Option<u64> {
    if !third_party_credentialed_fields(provider, api_key) {
        return None;
    }
    match provider {
        Some(Provider::Alibaba) => match console {
            Some(c) => Some(digest_fields(&[
                b"alibaba",
                c.token.as_bytes(),
                c.site.as_str().as_bytes(),
                c.region.as_bytes(),
            ])),
            None => Some(digest_fields(&[b"alibaba", b"no-console"])),
        },
        Some(p) => Some(digest_fields(&[
            b"known",
            provider_bytes(p),
            api_key.unwrap_or_default().trim().as_bytes(),
        ])),
        None => Some(digest_fields(&[
            b"generic",
            api_key.unwrap_or_default().trim().as_bytes(),
            base_url.map(str::trim).unwrap_or_default().as_bytes(),
        ])),
    }
}

/// The codex half's identity: the `auth.json` account id, read guard-free.
pub(crate) fn read_codex_identity(name: &ProfileName) -> Option<String> {
    crate::codex_auth::read_store_auth(name.as_str())
        .and_then(|auth| auth.account_id().map(str::to_string))
}

/// The throughput identity: the account uuid for an anchored OAuth spawn, the
/// stored-token digest for an unanchored one, else a digest of (effective
/// base_url, key or env token) — what a delegate spawn actually sends. A hybrid
/// (OAuth + endpoint auth) runs delegates on the key/env, so its OAuth log out
/// keeps the throughput record.
pub(crate) fn throughput_identity(
    oauth: &OAuthIdentity,
    base_url: Option<&str>,
    api_key: Option<&str>,
    env_token: Option<&str>,
) -> Option<u64> {
    let endpoint_auth =
        api_key.is_some_and(|k| !k.trim().is_empty()) || env_token.is_some_and(|t| !t.is_empty());
    if endpoint_auth || base_url.is_some_and(|u| !u.trim().is_empty()) {
        let key_or_env = api_key
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .or_else(|| env_token.map(|t| t.trim()).filter(|t| !t.is_empty()))
            .unwrap_or_default();
        return Some(digest_fields(&[
            b"endpoint",
            base_url.map(str::trim).unwrap_or_default().as_bytes(),
            key_or_env.as_bytes(),
        ]));
    }
    match (&oauth.anchor, oauth.stored) {
        (Some(uuid), _) => Some(digest_fields(&[b"anchor", uuid.trim().as_bytes()])),
        (None, Some(token)) => Some(digest_fields(&[b"oauth-token", &token.to_le_bytes()])),
        (None, None) => None,
    }
}

/// Every stored identity a change site compares; the endpoint fields ride
/// along so the AFTER computation reads the same sources as the BEFORE.
#[derive(Debug, Clone)]
pub(crate) struct BeforeIdentities {
    pub(crate) oauth: OAuthIdentity,
    pub(crate) third_party: Option<u64>,
    pub(crate) throughput: Option<u64>,
    pub(crate) base_url: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) env_token: Option<String>,
    pub(crate) has_credentials: bool,
}

/// Read `name`'s stored identities from the raw files — OAuth, third-party,
/// throughput, and the endpoint fields the throughput identity consumes.
pub(crate) fn read_before_identities(name: &ProfileName) -> BeforeIdentities {
    let oauth = read_oauth_identity(name);
    let endpoint = stored_endpoint_identity(name);
    let has_credentials = crate::profile::stored_credentials_file_exists(name);
    let (base_url, api_key, env_token) = match &endpoint {
        Some(e) => (
            e.base_url.clone(),
            e.api_key.clone(),
            crate::profile::inference_env_token(&e.env),
        ),
        None => (None, None, None),
    };
    let third_party = endpoint.as_ref().and_then(|e| {
        let provider = e.base_url.as_deref().and_then(Provider::from_base_url);
        third_party_identity(
            provider,
            e.api_key.as_deref(),
            e.console.as_ref(),
            e.base_url.as_deref(),
        )
    });
    let throughput = throughput_identity(
        &oauth,
        base_url.as_deref(),
        api_key.as_deref(),
        env_token.as_deref(),
    );
    BeforeIdentities {
        oauth,
        third_party,
        throughput,
        base_url,
        api_key,
        env_token,
        has_credentials,
    }
}

/// Read every half a snapshot holds for `name`, straight from the raw files.
/// Throughput is absent by design: it has no memory store to forget.
pub(crate) fn read_snapshot_identities(name: &ProfileName) -> SnapshotIdentities {
    let before = read_before_identities(name);
    SnapshotIdentities {
        oauth: before.oauth,
        third_party: before.third_party,
        codex: read_codex_identity(name),
        ..SnapshotIdentities::default()
    }
}

/// Which halves' identity files exist but cannot be read or parsed — the halves
/// the compare skips rather than reading as "no credential".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CompareSkips {
    oauth: bool,
    third_party: bool,
    codex: bool,
}

impl CompareSkips {
    fn any(self) -> bool {
        self.oauth || self.third_party || self.codex
    }

    /// The unreadable flag for one half.
    fn half(self, half: Half) -> bool {
        match half {
            Half::OAuth => self.oauth,
            Half::ThirdParty => self.third_party,
            Half::Codex => self.codex,
            Half::Throughput => false,
        }
    }
}

/// Read `name`'s identity for the compare, marking each half whose identity
/// file exists but cannot be read or parsed. Such a half is skipped — a torn
/// write is RETRY, never "no credential" — so the compare forgets nothing on a
/// guess and the fingerprint stays unadopted.
fn read_for_compare(name: &ProfileName) -> (SnapshotIdentities, CompareSkips) {
    let (oauth, oauth_skip) = read_oauth_for_compare(name);
    let (third_party, third_party_skip) = read_third_party_for_compare(name);
    let (codex, codex_skip) = read_codex_for_compare(name);
    (
        SnapshotIdentities {
            oauth,
            third_party,
            codex,
            ..SnapshotIdentities::default()
        },
        CompareSkips {
            oauth: oauth_skip,
            third_party: third_party_skip,
            codex: codex_skip,
        },
    )
}

/// The OAuth half's identity plus its unreadable flag, off ONE read of
/// `credentials.json` and, when that holds no pair, one of
/// `session-token.json`. A store that exists but fails to read or parse is
/// unreadable; a store that parses with no login is genuinely "no credential".
pub(crate) fn read_oauth_for_compare(name: &ProfileName) -> (OAuthIdentity, bool) {
    let (identity, unreadable) = read_pair_for_compare(name);
    if unreadable || identity.stored.is_some() {
        return (identity, unreadable);
    }
    let (setup_token, unreadable) = read_setup_token_for_compare(name);
    (
        OAuthIdentity {
            setup_token,
            ..identity
        },
        unreadable,
    )
}

/// [`read_oauth_for_compare`]'s pair half alone: the identity the throughput
/// half derives from, which a setup token is no part of.
fn read_pair_for_compare(name: &ProfileName) -> (OAuthIdentity, bool) {
    let Ok(path) = crate::profile::profile_subpath(name, "credentials.json") else {
        return (OAuthIdentity::default(), false);
    };
    let creds = crate::profile::read_json_file::<crate::profile::ClaudeCredentials>(&path);
    let stored = match creds {
        Ok(creds) => creds
            .claude_ai_oauth
            .map(|o| stored_token_digest(&o.access_token)),
        Err(_) if path.exists() => return (OAuthIdentity::default(), true),
        Err(_) => None,
    };
    let anchor = match stored {
        Some(_) => load_profile_cache::<AccountId>(name, ACCOUNT_ID_CACHE_FILE)
            .filter(|u| !u.trim().is_empty()),
        None => None,
    };
    (
        OAuthIdentity {
            anchor,
            stored,
            setup_token: None,
        },
        false,
    )
}

/// The third-party half's identity plus its unreadable flag. The identity comes
/// off one `config.toml` read; a store that exists but fails to read or parse
/// is unreadable, never "no credential".
pub(crate) fn read_third_party_for_compare(name: &ProfileName) -> (Option<u64>, bool) {
    let Ok(path) = crate::profile::profile_subpath(name, "config.toml") else {
        return (None, false);
    };
    let Some(endpoint) = crate::profile::stored_endpoint_identity(name) else {
        return (None, path.exists());
    };
    let provider = endpoint
        .base_url
        .as_deref()
        .and_then(Provider::from_base_url);
    let identity = third_party_identity(
        provider,
        endpoint.api_key.as_deref(),
        endpoint.console.as_ref(),
        endpoint.base_url.as_deref(),
    );
    (identity, false)
}

/// The codex half's identity plus its unreadable flag. `auth.json` is rewritten
/// in place by codex outside clauth's state flock, so a torn read here is the
/// live case: it reads unreadable, never "no credential".
pub(crate) fn read_codex_for_compare(name: &ProfileName) -> (Option<String>, bool) {
    let Ok(path) = crate::profile::profile_subpath(name, "auth.json") else {
        return (None, false);
    };
    match crate::codex_auth::read_store_auth(name.as_str()) {
        Some(auth) => (auth.account_id().map(str::to_string), false),
        None if path.exists() => (None, true),
        None => (None, false),
    }
}

/// The throughput half's identity plus its unreadable flag, for the MCP
/// delegate-end compare. Reads `credentials.json` and `config.toml` lock-free;
/// either file existing but unreadable/unparseable marks the half unreadable, so
/// a torn write reads RETRY (write nothing) rather than "no credential".
pub(crate) fn read_throughput_for_compare(name: &ProfileName) -> (Option<u64>, bool) {
    let (oauth, oauth_unreadable) = read_pair_for_compare(name);
    let (base_url, api_key, env_token, endpoint_unreadable) =
        match crate::profile::profile_subpath(name, "config.toml") {
            Ok(path) => match crate::profile::stored_endpoint_identity(name) {
                Some(e) => (
                    e.base_url,
                    e.api_key,
                    crate::profile::inference_env_token(&e.env),
                    false,
                ),
                None if path.exists() => (None, None, None, true),
                None => (None, None, None, false),
            },
            Err(_) => (None, None, None, false),
        };
    let identity = throughput_identity(
        &oauth,
        base_url.as_deref(),
        api_key.as_deref(),
        env_token.as_deref(),
    );
    (identity, oauth_unreadable || endpoint_unreadable)
}

/// The throughput identity a delegate launch on `p` records under, applying the
/// spawn OAuth identity rule: `name`'s disk anchor is adopted only when the
/// stored token still equals the in-memory token. Reads the anchor and stored
/// token itself, so the capture is one consistent read — a delegate spawned
/// under a stale in-memory profile records an identity that compares as changed
/// against the stored credential.
pub(crate) fn throughput_identity_for_spawn(
    name: &ProfileName,
    p: &crate::profile::Profile,
) -> Option<u64> {
    let access = p
        .credentials
        .as_ref()
        .and_then(|c| c.claude_ai_oauth.as_ref())
        .map(|o| o.access_token.as_str());
    let oauth = match access {
        Some(access) => spawn_oauth_identity(name, access),
        None => OAuthIdentity::default(),
    };
    let env_token = crate::profile::inference_env_token(&p.env);
    throughput_identity(
        &oauth,
        p.base_url.as_deref(),
        p.api_key.as_deref(),
        env_token.as_deref(),
    )
}

/// Whether `half` differs between two snapshots. The OAuth half uses the stored
/// changed predicate (an anchor backfill over the same token is not a change);
/// the other halves compare by equality.
fn half_changed(half: Half, before: &SnapshotIdentities, after: &SnapshotIdentities) -> bool {
    match half {
        Half::OAuth => oauth_changed(&before.oauth, &after.oauth),
        Half::ThirdParty => before.third_party != after.third_party,
        Half::Codex => before.codex != after.codex,
        Half::Throughput => false,
    }
}

/// The names a snapshot inspects: the config's profiles plus the codex roster
/// (a codex-only profile lives in `codex-profiles.toml`, not `profiles.toml`).
/// A failed roster read stands `fallback` in for the codex names — the previous
/// snapshot's keys — so a failed read forgets no codex names on a guess.
fn snapshot_names(
    config: &[ProfileName],
    fallback: impl Iterator<Item = ProfileName>,
) -> Vec<ProfileName> {
    let mut names: Vec<ProfileName> = config.to_vec();
    match crate::codex_profiles::CodexState::load() {
        Ok(codex) => names.extend(codex.profiles().iter().cloned()),
        Err(_) => names.extend(fallback),
    }
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    names
}

/// Build a fresh snapshot for a process that just registered its reading
/// stores, seeded from disk so the first compare sees no spurious change.
pub(crate) fn new_snapshot(
    config: &[ProfileName],
) -> Arc<RankedMutex<SnapshotMap, rank::Snapshot>> {
    let snapshot: Arc<RankedMutex<SnapshotMap, rank::Snapshot>> =
        Arc::new(RankedMutex::new(HashMap::new()));
    seed_snapshot_handle(&snapshot, config);
    snapshot
}

fn seed_snapshot_handle(
    snapshot: &RankedMutex<SnapshotMap, rank::Snapshot>,
    config: &[ProfileName],
) {
    let names = snapshot_names(config, std::iter::empty());
    let mut map = snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for name in &names {
        map.insert(name.to_string(), read_snapshot_identities(name));
    }
}

/// The throughput identity a change site is about to write. `base_url` is the
/// RAW managed field — the effective base_url is derived like `load_profile`
/// does, so the site and the reload agree on what the delegates run on.
pub(crate) fn throughput_for_endpoint(
    oauth: &OAuthIdentity,
    base_url: Option<&str>,
    has_credentials: bool,
    api_key: Option<&str>,
    env: &std::collections::BTreeMap<String, String>,
) -> Option<u64> {
    let effective = crate::profile::effective_base_url(
        base_url.map(str::to_string),
        has_credentials,
        api_key,
        env,
    );
    let env_token = crate::profile::inference_env_token(env);
    throughput_identity(oauth, effective.as_deref(), api_key, env_token.as_deref())
}

/// The changed halves for an endpoint write: the third-party and throughput
/// identities of the values about to be written. `endpoint.base_url` is the RAW
/// managed field, what [`throughput_for_endpoint`] needs. `oauth` is the AFTER
/// identity (`before.oauth` when the OAuth half is untouched).
pub(crate) fn endpoint_identity_plan(
    before: &BeforeIdentities,
    oauth: &OAuthIdentity,
    endpoint: &StoredEndpointIdentity,
    has_credentials: bool,
) -> ForgetPlan {
    let provider = endpoint
        .base_url
        .as_deref()
        .and_then(Provider::from_base_url);
    let third_party = third_party_identity(
        provider,
        endpoint.api_key.as_deref(),
        endpoint.console.as_ref(),
        endpoint.base_url.as_deref(),
    );
    let throughput = throughput_for_endpoint(
        oauth,
        endpoint.base_url.as_deref(),
        has_credentials,
        endpoint.api_key.as_deref(),
        &endpoint.env,
    );
    ForgetPlan {
        oauth: oauth_changed(&before.oauth, oauth),
        third_party: before.third_party != third_party,
        throughput: before.throughput != throughput,
        ..ForgetPlan::default()
    }
}

/// The per-process forget epoch: monotonic per `(name, half)`, never removed,
/// so a delete-then-recreate cannot reuse a value. Ranked [`rank::Epoch`] (160).
type EpochMap = HashMap<(String, Half), u64>;

static EPOCH: LazyLock<RankedMutex<EpochMap, rank::Epoch>> =
    LazyLock::new(|| RankedMutex::new(HashMap::new()));

/// The current epoch for `(name, half)`. A seed reads this before its disk read
/// as its pre-read capture; the write-side compare runs under [`epoch_guard`].
pub(crate) fn epoch_value(name: &ProfileName, half: Half) -> u64 {
    EPOCH
        .lock()
        .ok()
        .and_then(|m| m.get(&(name.to_string(), half)).copied())
        .unwrap_or(0)
}

/// Acquire the epoch guard, held from a seed's compare through its writes so a
/// forget cannot bump the epoch between check and write. Also held by the
/// tick's partition across the token-list clone + epoch capture, so a forget
/// cannot land between them and stamp a stale entry's landing as unmoved.
pub(crate) fn epoch_guard() -> crate::lockorder::RankedGuard<'static, EpochMap> {
    EPOCH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Compare `(name, half)`'s epoch under the guard and return it held when it is
/// still `epoch`, so the caller keeps it through its writes.
pub(crate) fn epoch_guard_if_current(
    name: &ProfileName,
    half: Half,
    epoch: u64,
) -> Option<crate::lockorder::RankedGuard<'static, EpochMap>> {
    let guard = epoch_guard();
    (guard.get(&(name.to_string(), half)).copied().unwrap_or(0) == epoch).then_some(guard)
}

pub(crate) fn bump_epoch(name: &ProfileName, half: Half) {
    if let Ok(mut m) = EPOCH.lock() {
        let e = m.entry((name.to_string(), half)).or_insert(0);
        *e = e.wrapping_add(1);
    }
}

/// Test seam: try to acquire the epoch guard, returning `None` while a seed
/// holds it. Pins that the seed holds the guard across its compare and writes.
#[cfg(test)]
pub(crate) fn try_lock_epoch_for_test() -> Option<crate::lockorder::RankedGuard<'static, EpochMap>>
{
    EPOCH.try_lock().ok()
}

/// Drop one half's disk caches. Called inside the change site's state-flock
/// hold, BEFORE the identity writes, so a reloading process that re-seeds from
/// disk after the writes finds the old cache already gone.
pub(crate) fn forget_disk(name: &ProfileName, half: Half) {
    match half {
        Half::OAuth => {
            remove_profile_cache(name, USAGE_CACHE_FILE);
            remove_profile_cache(name, PROFILE_FETCHED_CACHE_FILE);
            remove_profile_cache(name, KICK_BLOCK_CACHE_FILE);
        }
        Half::ThirdParty => {
            // `third_party_auth.json` stays: its dead-key verdict is
            // fingerprint-bound, so a failed save that keeps the stored key
            // must keep its verdict.
            remove_profile_cache(name, THIRD_PARTY_CACHE_FILE);
        }
        Half::Codex => {
            // The codex reading shares `usage_cache.json` with the OAuth half;
            // an account change must drop the old account's reading now.
            remove_profile_cache(name, USAGE_CACHE_FILE);
        }
        Half::Throughput => {
            remove_profile_cache(name, crate::throughput::THROUGHPUT_CACHE_FILE);
        }
    }
}

/// A memory forget a change site noted for a store-holding process to apply
/// once its config guard drops. `Rename` moves entries — the disk half already
/// moved with the directory.
enum PendingAction {
    Forget(ProfileName, Half),
    Rename(ProfileName, ProfileName),
}

/// Forgets a change site noted for a store-holding process to apply to memory.
/// Process-global.
static PENDING_FORGETS: LazyLock<RankedMutex<Vec<PendingAction>, rank::ForgetQueue>> =
    LazyLock::new(|| RankedMutex::new(Vec::new()));

/// Whether THIS process built reading stores (and so drains the pending queue).
/// The CLI and the MCP server never drain the queue, so a process without
/// stores records nothing.
static STORES_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Mark that this process holds reading stores and will drain the pending
/// forgets.
pub(crate) fn register_memory_stores() {
    STORES_REGISTERED.store(true, Ordering::Release);
}

/// Record that `name`'s `half` changed. Called after the identity writes land.
pub(crate) fn note_forget(name: &ProfileName, half: Half) {
    if !STORES_REGISTERED.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut pending) = PENDING_FORGETS.lock() {
        pending.push(PendingAction::Forget(name.clone(), half));
    }
}

/// Record that `old` was renamed to `new`. Called after the record move lands.
pub(crate) fn note_rename(old: &ProfileName, new: &ProfileName) {
    if !STORES_REGISTERED.load(Ordering::Acquire) {
        return;
    }
    if let Ok(mut pending) = PENDING_FORGETS.lock() {
        pending.push(PendingAction::Rename(old.clone(), new.clone()));
    }
}

/// Test seam: drop every queued forget and the registered-stores flag, so a
/// change site that ran under one sandbox cannot leak a forget into the next
/// test's drain. Called from `HomeSandbox::drop`.
#[cfg(test)]
pub(crate) fn reset_pending_forgets_for_test() {
    if let Ok(mut pending) = PENDING_FORGETS.lock() {
        pending.clear();
    }
    STORES_REGISTERED.store(false, Ordering::Release);
}

/// The stores `forget_memory` clears, cloned by a store-holding process.
pub(crate) struct MemoryStores {
    pub(crate) usage: UsageStore,
    pub(crate) usage_status: StatusStore,
    pub(crate) last_fetched: LastFetchedAt,
    pub(crate) next_refresh: NextRefreshPerProfile,
    pub(crate) poll_streaks: PollStreaks,
    pub(crate) kick_blocks: KickBlocks,
    pub(crate) weekly_reset_kicks: WeeklyResetKicks,
    pub(crate) auto_start_queue: AutoStartQueueState,
    pub(crate) third_party_usage: ThirdPartyUsageStore,
    pub(crate) third_party_status: ThirdPartyStatusStore,
    pub(crate) third_party_streaks: ThirdPartyStreaks,
    pub(crate) third_party_broken: ThirdPartyBroken,
    pub(crate) suppressed: SuppressedAuthExpiredStore,
    pub(crate) interval_ms: Arc<AtomicU64>,
    /// The per-process identity snapshot the reload compare diffs against,
    /// refreshed on every applied forget. Leaf-ranked so the compare can read
    /// it, release it, then forget into the store locks without inverting the
    /// lock order.
    pub(crate) snapshot: Arc<RankedMutex<SnapshotMap, rank::Snapshot>>,
}

/// The halves a snapshot holds per name. `Throughput` has no memory store, so
/// it is compared nowhere here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotIdentities {
    pub(crate) oauth: OAuthIdentity,
    pub(crate) third_party: Option<u64>,
    pub(crate) codex: Option<String>,
    /// A half whose refresh read could not be read (a torn identity file):
    /// its stored identity is unknown, so the partition skips every entry of
    /// that name (S2) until the next compare writes a real value (S3). The
    /// identity value beside it stays the last-known one — a torn write is
    /// RETRY, never "no credential". Codex has no token list (its leg compares
    /// `auth.json` itself), so it carries no unknown mark.
    pub(crate) oauth_unknown: bool,
    pub(crate) third_party_unknown: bool,
}

impl SnapshotIdentities {
    /// Whether `half` is marked unknown — the partition's skip signal.
    pub(crate) fn unknown(&self, half: Half) -> bool {
        match half {
            Half::OAuth => self.oauth_unknown,
            Half::ThirdParty => self.third_party_unknown,
            Half::Codex | Half::Throughput => false,
        }
    }
}

pub(crate) type SnapshotMap = HashMap<String, SnapshotIdentities>;

/// What a reload compare found: whether it forgot a change this process's own
/// drained queue did not account for, and whether it skipped anything.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompareOutcome {
    /// A half whose identity changed and was NOT this process's own drained
    /// forget (an external change), or a vanished name with a non-default half.
    external_change: bool,
    /// The compare skipped at least one half: an identity file that exists but
    /// cannot be read or parsed.
    skipped: bool,
}

impl CompareOutcome {
    /// Whether a non-reload re-baseline may adopt the new fingerprint. It may
    /// not after an external change or a skip: the in-memory config and token
    /// lists still match the OLD fingerprint then, so the next tick's reload
    /// must re-read config and rebuild the token lists.
    pub(crate) fn clean(self) -> bool {
        !self.external_change && !self.skipped
    }
}

#[cfg(test)]
impl MemoryStores {
    /// Empty stores, for a test that drives a route or drain without a real
    /// scheduler behind it. Does NOT register stores: registration is the
    /// change-site gate, and a test that runs no change site needs none.
    pub(crate) fn empty_for_test() -> Self {
        use std::collections::HashSet;
        MemoryStores {
            usage: Arc::new(RankedMutex::new(HashMap::new())),
            usage_status: Arc::new(RankedMutex::new(HashMap::new())),
            last_fetched: Arc::new(RankedMutex::new(HashMap::new())),
            next_refresh: Arc::new(RankedMutex::new(HashMap::new())),
            poll_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            kick_blocks: Arc::new(RankedMutex::new(HashMap::new())),
            weekly_reset_kicks: Arc::new(RankedMutex::new(HashSet::new())),
            auto_start_queue: super::new_auto_start_queue_state(),
            third_party_usage: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_status: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_streaks: Arc::new(RankedMutex::new(HashMap::new())),
            third_party_broken: Arc::new(RankedMutex::new(HashMap::new())),
            suppressed: Arc::new(RankedMutex::new(HashMap::new())),
            interval_ms: Arc::new(AtomicU64::new(90_000)),
            snapshot: Arc::new(RankedMutex::new(HashMap::new())),
        }
    }

    /// Seed the snapshot from disk for the names a test is about to drive, so
    /// the first compare treats the pre-test identity as the baseline (the same
    /// moment the real `App`/`Daemon` construction seeds at).
    pub(crate) fn seed_snapshot(&self, config: &[ProfileName]) {
        seed_snapshot_handle(&self.snapshot, config);
    }
}

/// Bump the epoch, clear the half's name-keyed entries, then re-seed from disk
/// (a seed another process wrote survives). Memory only — the durable
/// `/profile` stamp is `forget_disk(OAuth)`'s. Runs with no ranked lock held
/// (the stores rank BELOW `Config`).
pub(crate) fn forget_memory(stores: &MemoryStores, name: &ProfileName, half: Half) {
    // Read the half's identity BEFORE the re-seed through the compare's
    // unreadable-aware readers: a torn file leaves the snapshot half untouched
    // (RETRY, never "no credential"), and a change by another process between
    // the read and the re-seed is not absorbed into the snapshot without a
    // forget.
    let refreshed = read_half_identity(name, half);
    forget_memory_with_identity(stores, name, half, refreshed);
}

/// The forget body: bump the epoch, clear the half's name-keyed entries, then
/// re-seed from disk (a seed another process wrote survives). `refreshed` is the
/// identity to store into the snapshot refresh; `None` (an unreadable file, or
/// Throughput, which has no snapshot entry) leaves the snapshot half untouched.
fn forget_memory_with_identity(
    stores: &MemoryStores,
    name: &ProfileName,
    half: Half,
    refreshed: Option<HalfIdentity>,
) {
    // Snapshot half FIRST: the tick's partition decides "moved" by comparing
    // each entry's identity against this snapshot half while holding the epoch
    // guard, so the half must never lag its epoch — write it before the bump,
    // else a partition landing between the two reads a stale snapshot against
    // the post-bump epoch and pairs the old entry with it (P1).
    refresh_snapshot_half(stores, name, half, refreshed);
    bump_epoch(name, half);
    clear_memory(stores, name, half);
    let interval_ms = stores.interval_ms.load(Ordering::Relaxed);
    let now = now_ms();
    match half {
        Half::OAuth => {
            let _ = try_seed_cache(
                &stores.usage,
                &stores.usage_status,
                &stores.last_fetched,
                name,
                now,
                interval_ms,
            );
        }
        Half::ThirdParty => {
            seed_third_party_name(
                &stores.third_party_usage,
                &stores.third_party_status,
                &stores.last_fetched,
                name,
                false,
                now,
                interval_ms,
            );
        }
        Half::Codex => {
            // Re-seed from the shared cache: a same-name claude profile's
            // reading comes back instead of staying blank until its next fetch.
            seed_codex_name(&stores.usage, &stores.usage_status, name, now, interval_ms);
        }
        Half::Throughput => {}
    }
}

/// One half's identity read before a forget's re-seed — the value the snapshot
/// refresh stores, so the refresh never disagrees with the re-seed's disk read.
enum HalfIdentity {
    OAuth(OAuthIdentity),
    ThirdParty(Option<u64>),
    Codex(Option<String>),
}

fn read_half_identity(name: &ProfileName, half: Half) -> Option<HalfIdentity> {
    match half {
        Half::OAuth => {
            let (identity, unreadable) = read_oauth_for_compare(name);
            (!unreadable).then_some(HalfIdentity::OAuth(identity))
        }
        Half::ThirdParty => {
            let (identity, unreadable) = read_third_party_for_compare(name);
            (!unreadable).then_some(HalfIdentity::ThirdParty(identity))
        }
        Half::Codex => {
            let (identity, unreadable) = read_codex_for_compare(name);
            (!unreadable).then_some(HalfIdentity::Codex(identity))
        }
        Half::Throughput => None,
    }
}

/// One half's identity from the compare's flock-held reads, for the snapshot
/// refresh after a compare forget: `None` when the half was unreadable (leave
/// the snapshot half untouched) or has no snapshot entry.
fn half_identity_from_reads(
    reads: &HashMap<String, (SnapshotIdentities, CompareSkips)>,
    name: &ProfileName,
    half: Half,
) -> Option<HalfIdentity> {
    let (identity, unreadable) = reads.get(name.as_str())?;
    if unreadable.half(half) {
        return None;
    }
    match half {
        Half::OAuth => Some(HalfIdentity::OAuth(identity.oauth.clone())),
        Half::ThirdParty => Some(HalfIdentity::ThirdParty(identity.third_party)),
        Half::Codex => Some(HalfIdentity::Codex(identity.codex.clone())),
        Half::Throughput => None,
    }
}

/// Store the half's pre-forget identity into the snapshot, so this process's
/// own change reads as "already seen" at the next compare and is forgotten
/// exactly once. Runs BEFORE the epoch bump (the snapshot half must never lag
/// its epoch: the partition reads both under the epoch guard, so a half written
/// after the bump pairs an old entry with the post-bump epoch).
///
/// `refreshed = None` means the half's identity could not be read (a torn
/// identity file): the snapshot half is then marked UNKNOWN rather than left
/// with a stale value, so the partition skips every entry of that name (S2)
/// until the next compare writes a real value (S3). `Throughput` has no
/// snapshot entry, so its `None` is nothing to mark.
fn refresh_snapshot_half(
    stores: &MemoryStores,
    name: &ProfileName,
    half: Half,
    refreshed: Option<HalfIdentity>,
) {
    let mut snap = stores
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = snap.entry(name.to_string()).or_default();
    let Some(refreshed) = refreshed else {
        if half != Half::Throughput {
            match half {
                Half::OAuth => entry.oauth_unknown = true,
                Half::ThirdParty => entry.third_party_unknown = true,
                Half::Codex | Half::Throughput => {}
            }
        }
        return;
    };
    match (half, refreshed) {
        (Half::OAuth, HalfIdentity::OAuth(v)) => {
            entry.oauth = v;
            entry.oauth_unknown = false;
        }
        (Half::ThirdParty, HalfIdentity::ThirdParty(v)) => {
            entry.third_party = v;
            entry.third_party_unknown = false;
        }
        (Half::Codex, HalfIdentity::Codex(v)) => {
            entry.codex = v;
        }
        _ => {}
    }
}

fn clear_memory(stores: &MemoryStores, name: &ProfileName, half: Half) {
    match half {
        Half::OAuth => {
            remove_name_entry(&stores.usage, name);
            remove_name_entry(&stores.usage_status, name);
            remove_name_entry(&stores.poll_streaks, name);
            remove_leg_entry(&stores.last_fetched, name, FetchLeg::OAuth);
            remove_leg_entry(&stores.next_refresh, name, FetchLeg::OAuth);
            remove_name_entry(&stores.kick_blocks, name);
            stores
                .weekly_reset_kicks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(name);
            super::forget_election_failures(&stores.auto_start_queue, name);
            forget_profile_memo(name);
        }
        Half::ThirdParty => {
            remove_name_entry(&stores.third_party_usage, name);
            remove_name_entry(&stores.third_party_status, name);
            remove_name_entry(&stores.third_party_streaks, name);
            remove_name_entry(&stores.third_party_broken, name);
            remove_leg_entry(&stores.last_fetched, name, FetchLeg::ThirdParty);
            remove_leg_entry(&stores.next_refresh, name, FetchLeg::ThirdParty);
            remove_name_entry(&stores.suppressed, name);
        }
        Half::Codex => {
            remove_name_entry(&stores.usage, name);
            remove_name_entry(&stores.usage_status, name);
            forget_codex_polled(name.as_str());
        }
        Half::Throughput => {}
    }
}

/// Clear what the usage probe left for `name` once nothing reads the account,
/// so it shows as a restart shows it, where the seed leaves it out: the
/// reading, the last probe's status and its countdown go, or the frozen
/// figures would read `up to date`, and a refused token rests no longer, so a
/// probe switched back on tries it once. The cadence stamp stays.
pub(crate) fn forget_probe_reading(stores: &MemoryStores, name: &ProfileName) {
    remove_name_entry(&stores.usage, name);
    remove_name_entry(&stores.usage_status, name);
    remove_leg_entry(&stores.next_refresh, name, FetchLeg::OAuth);
    remove_name_entry(&stores.suppressed, name);
}

/// Take the pending queue (forgets + renames) without applying it, so the
/// compare can apply it AFTER its flock-held reads. The queue lock ranks above
/// the state flock, so the compare takes it inside the hold (ascending).
fn take_pending_forgets() -> Vec<PendingAction> {
    PENDING_FORGETS
        .lock()
        .ok()
        .map(|mut p| std::mem::take(&mut *p))
        .unwrap_or_default()
}

/// Drain the forgets and renames a change site noted and apply each to memory.
/// Called by the TUI after its config guard drops (the change site itself
/// cannot: the stores rank below `Config`).
pub(crate) fn apply_pending_forgets(stores: &MemoryStores) {
    let pending: Vec<PendingAction> = take_pending_forgets();
    for action in pending {
        match action {
            PendingAction::Forget(name, half) => forget_memory(stores, &name, half),
            PendingAction::Rename(old, new) => forget_memory_rename(stores, &old, &new),
        }
    }
}

/// The shared reload compare: for each name in the snapshot's union, forget the
/// halves whose stored identity changed since this process last looked. Memory
/// only — it never writes disk. `config` is the freshly-loaded config's profile
/// names; the codex roster and the snapshot's own keys join it.
pub(crate) fn reload_and_forget(stores: &MemoryStores, config: &[ProfileName]) -> CompareOutcome {
    // Take and apply this process's pending queue first. Each taken forget is
    // forgotten once and its snapshot half refreshed from a fresh
    // unreadable-aware read (an unreadable file leaves the half untouched), so
    // the diff below reads the half as already seen and never counts it as an
    // external change; a taken rename moves the snapshot entry with the memory
    // entries. No state flock: the anchor's remove-then-write gap is closed at
    // its writers, so the compare needs no hold.
    let taken: Vec<PendingAction> = take_pending_forgets();
    let mut taken_renames: Vec<(ProfileName, ProfileName)> = Vec::new();
    for action in &taken {
        match action {
            PendingAction::Forget(name, half) => forget_memory(stores, name, *half),
            PendingAction::Rename(old, new) => {
                taken_renames.push((old.clone(), new.clone()));
                forget_memory_rename(stores, old, new);
            }
        }
    }

    // The snapshot's previous keys, for the codex-roster fallback, and the
    // names to read identities for. Read AFTER the taken queue applied, so the
    // keys already reflect any taken rename.
    let read_prev_keys: Vec<ProfileName> = {
        let snap = stores
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        snap.keys().map(|k| ProfileName::from(k.clone())).collect()
    };
    let read_names: Vec<ProfileName> = snapshot_names(config, read_prev_keys.iter().cloned());
    let reads: HashMap<String, (SnapshotIdentities, CompareSkips)> = read_names
        .iter()
        .map(|n| (n.to_string(), read_for_compare(n)))
        .collect();

    // The present-name set is the read set with the taken renames applied —
    // never a second roster read, which could disagree with the reads.
    let mut present = read_names;
    for (old, new) in &taken_renames {
        present.retain(|n| n != old);
        if !present.contains(new) {
            present.push(new.clone());
        }
    }
    present.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    present.dedup();
    let present_set: HashSet<String> = present.iter().map(|n| n.to_string()).collect();

    // Diff the reads against the snapshot, skipping unreadable halves; then
    // forget each changed half and the vanished names' non-default halves. The
    // store locks rank below the snapshot lock, so the forgets run after it
    // released.
    let mut skipped = false;
    let mut external_change = false;
    let (forgets, vanished, before_map) = {
        let snap = stores
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut forgets: Vec<(ProfileName, Half)> = Vec::new();
        let mut vanished: Vec<(String, SnapshotIdentities)> = Vec::new();
        let mut before_map: HashMap<String, SnapshotIdentities> = HashMap::new();
        for name in &present {
            let Some((after, unreadable)) = reads.get(name.as_str()) else {
                continue;
            };
            skipped |= unreadable.any();
            let before = snap.get(name.as_str()).cloned().unwrap_or_default();
            before_map.insert(name.to_string(), before.clone());
            for half in [Half::OAuth, Half::ThirdParty, Half::Codex] {
                if unreadable.half(half) {
                    continue;
                }
                if half_changed(half, &before, after) {
                    external_change = true;
                    forgets.push((name.clone(), half));
                }
            }
        }
        for (name, entry) in snap.iter() {
            if !present_set.contains(name) {
                vanished.push((name.clone(), entry.clone()));
            }
        }
        (forgets, vanished, before_map)
    };

    for (name, half) in &forgets {
        let refreshed = half_identity_from_reads(&reads, name, *half);
        forget_memory_with_identity(stores, name, *half, refreshed);
    }
    for (name, entry) in &vanished {
        let name = ProfileName::from(name.clone());
        if entry.oauth != OAuthIdentity::default() {
            external_change = true;
            forget_memory(stores, &name, Half::OAuth);
        }
        if entry.third_party.is_some() {
            external_change = true;
            forget_memory(stores, &name, Half::ThirdParty);
        }
        if entry.codex.is_some() {
            external_change = true;
            forget_memory(stores, &name, Half::Codex);
        }
    }

    // Re-lock to drop vanished names and write the compare's reads per half.
    let mut snap = stores
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    snap.retain(|name, _| present_set.contains(name));
    for name in &present {
        let Some((after, unreadable)) = reads.get(name.as_str()) else {
            continue;
        };
        let before = before_map.get(name.as_str()).cloned().unwrap_or_default();
        write_snapshot_half_cas(&mut snap, name.as_str(), &before, after, unreadable);
    }

    CompareOutcome {
        external_change,
        skipped,
    }
}

/// Write the compare's read identity for one name into the snapshot, per half.
/// Each half write happens only where the half still equals the value the diff
/// read (compare-and-set), so a concurrent forget's refresh is kept rather than
/// overwritten. A half the compare marked unreadable is not written: its
/// snapshot half keeps its current value (a torn write is RETRY, never
/// "no credential"). A half the compare DID write clears the unknown mark (S3):
/// the next compare's read is the value the partition can trust again.
fn write_snapshot_half_cas(
    snap: &mut SnapshotMap,
    name: &str,
    before: &SnapshotIdentities,
    after: &SnapshotIdentities,
    unreadable: &CompareSkips,
) {
    let entry = snap.entry(name.to_string()).or_default();
    if !unreadable.oauth && entry.oauth == before.oauth {
        entry.oauth = after.oauth.clone();
        entry.oauth_unknown = false;
    }
    if !unreadable.third_party && entry.third_party == before.third_party {
        entry.third_party = after.third_party;
        entry.third_party_unknown = false;
    }
    if !unreadable.codex && entry.codex == before.codex {
        entry.codex = after.codex.clone();
    }
}

/// Move `old`'s in-memory entries to `new` and bump the OLD name's epochs so a
/// stale in-flight landing or seed writes nothing.
pub(crate) fn forget_memory_rename(stores: &MemoryStores, old: &ProfileName, new: &ProfileName) {
    bump_epoch(old, Half::OAuth);
    bump_epoch(old, Half::ThirdParty);
    bump_epoch(old, Half::Codex);
    move_name_entry(&stores.usage, old, new);
    move_name_entry(&stores.usage_status, old, new);
    move_name_entry(&stores.third_party_usage, old, new);
    move_name_entry(&stores.third_party_status, old, new);
    move_name_entry(&stores.third_party_streaks, old, new);
    move_name_entry(&stores.third_party_broken, old, new);
    move_name_entry(&stores.poll_streaks, old, new);
    move_name_entry(&stores.kick_blocks, old, new);
    move_name_entry(&stores.suppressed, old, new);
    move_leg_entries(&stores.last_fetched, old, new);
    move_leg_entries(&stores.next_refresh, old, new);
    move_name_set(&stores.weekly_reset_kicks, old, new);
    super::move_election_failures(&stores.auto_start_queue, old, new);
    // The durable `/profile` stamp rode the directory to `new`; only the old
    // name's memo is left, so `new` re-reads the moved stamp fresh.
    forget_profile_memo(old);
    // Move the snapshot entry too: the identity rode the directory, so the
    // compare must not read the rename as old-vanished + new-appeared.
    let mut snap = stores
        .snapshot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(entry) = snap.remove(old.as_str()) {
        snap.insert(new.to_string(), entry);
    }
}

fn move_name_entry<T, R: crate::lockorder::Rank>(
    map: &RankedMutex<HashMap<String, T>, R>,
    old: &ProfileName,
    new: &ProfileName,
) {
    let mut m = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(value) = m.remove(old.as_str()) {
        m.insert(new.to_string(), value);
    }
}

fn move_leg_entries<T, R: crate::lockorder::Rank>(
    map: &RankedMutex<HashMap<super::LegKey, T>, R>,
    old: &ProfileName,
    new: &ProfileName,
) {
    let mut m = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for leg in [FetchLeg::OAuth, FetchLeg::ThirdParty] {
        if let Some(value) = m.remove(&leg.key(old.clone())) {
            m.insert(leg.key(new.clone()), value);
        }
    }
}

fn move_name_set<R: crate::lockorder::Rank>(
    set: &RankedMutex<std::collections::HashSet<ProfileName>, R>,
    old: &ProfileName,
    new: &ProfileName,
) {
    let mut s = set
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if s.remove(old) {
        s.insert(new.clone());
    }
}

pub(super) fn remove_name_entry<T, R: crate::lockorder::Rank>(
    map: &RankedMutex<HashMap<String, T>, R>,
    name: &ProfileName,
) {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(name.as_str());
}

pub(super) fn remove_leg_entry<T, R: crate::lockorder::Rank>(
    map: &RankedMutex<HashMap<super::LegKey, T>, R>,
    name: &ProfileName,
    leg: FetchLeg,
) {
    map.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&leg.key(name.clone()));
}

/// Forget the changed halves' DISK caches, before the identity writes land.
pub(crate) fn forget_disk_plan(name: &ProfileName, plan: ForgetPlan) {
    for half in plan.changed_halves() {
        forget_disk(name, half);
    }
}

/// Note the changed halves for `forget_memory`, after the identity writes land.
pub(crate) fn note_forget_plan(name: &ProfileName, plan: ForgetPlan) {
    for half in plan.changed_halves().filter(|h| *h != Half::Throughput) {
        note_forget(name, half);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{OAuthIdentity, oauth_changed, third_party_identity};
    use crate::profile::AccountId;

    fn oa(anchor: Option<&str>, token: Option<&str>) -> OAuthIdentity {
        OAuthIdentity::for_values(anchor.map(AccountId::from).as_ref(), token)
    }

    #[test]
    fn an_anchor_appearing_over_an_unchanged_token_is_not_a_change() {
        let before = oa(None, Some("access-A"));
        let after = oa(Some("uuid-A"), Some("access-A"));
        assert!(
            !oauth_changed(&before, &after),
            "a backfill or adopt over the same stored token is not a change"
        );
    }

    #[test]
    fn an_anchor_removed_while_a_login_stays_is_a_change() {
        let before = oa(Some("uuid-A"), Some("access-A"));
        let after = oa(None, Some("access-A"));
        assert!(
            oauth_changed(&before, &after),
            "an unproven install drops the anchor while the login stays"
        );
    }

    #[test]
    fn an_anchor_move_is_a_change() {
        let before = oa(Some("uuid-A"), Some("access-A"));
        let after = oa(Some("uuid-B"), Some("access-A"));
        assert!(
            oauth_changed(&before, &after),
            "an anchor moving to another account is a change"
        );
    }

    #[test]
    fn an_unanchored_token_change_is_a_change() {
        let before = oa(None, Some("access-A"));
        let after = oa(None, Some("access-B"));
        assert!(
            oauth_changed(&before, &after),
            "a stored-token change on an unanchored profile is a change"
        );
    }

    #[test]
    fn an_anchored_rotation_is_not_a_change() {
        let before = oa(Some("uuid-A"), Some("access-A"));
        let after = oa(Some("uuid-A"), Some("access-B"));
        assert!(
            !oauth_changed(&before, &after),
            "a rotation on an anchored profile keeps the anchor"
        );
    }

    #[test]
    fn for_values_drops_an_anchor_without_a_login() {
        let no_login = OAuthIdentity::for_values(Some(&AccountId::from("uuid-A")), None);
        assert!(
            no_login.anchor.is_none() && no_login.stored.is_none(),
            "no stored login can carry an anchor"
        );
        let before = oa(Some("uuid-A"), Some("access-A"));
        assert!(
            oauth_changed(&before, &no_login),
            "a login disappearing reads as a change even when the anchor name survives"
        );
    }

    /// Every move of a setup token on a profile with no pair stored, and a
    /// pair appearing over one or going with one left behind: the token is
    /// that profile's identity until a pair takes over.
    #[test]
    fn a_setup_token_moves_the_identity_of_a_profile_without_a_pair() {
        let none = OAuthIdentity::default();
        let mint_a = OAuthIdentity::for_setup_token("setup-A");
        let mint_b = OAuthIdentity::for_setup_token("setup-B");
        let cases = [
            (&none, &mint_a, true, "a setup token appears"),
            (&mint_a, &none, true, "the setup token goes"),
            (&mint_a, &mint_b, true, "a re-mint"),
            (&mint_a, &mint_a, false, "the same mint written again"),
        ];
        for (before, after, changed, what) in cases {
            assert_eq!(oauth_changed(before, after), changed, "{what}");
        }
        for pair in [
            oa(None, Some("access-A")),
            oa(Some("uuid-A"), Some("access-A")),
        ] {
            assert!(
                oauth_changed(&mint_a, &pair),
                "a pair appears over a setup token: {pair:?}"
            );
            assert!(
                oauth_changed(&pair, &mint_a),
                "the pair goes and the setup token is left: {pair:?}"
            );
        }
    }

    #[test]
    fn a_generic_key_edit_changes_the_third_party_identity() {
        let before = third_party_identity(None, Some("key-A"), None, Some("https://gw.example"));
        let after = third_party_identity(None, Some("key-B"), None, Some("https://gw.example"));
        assert_ne!(
            before, after,
            "a generic key edit over the same base_url is a change"
        );
    }

    #[test]
    fn a_generic_base_url_edit_changes_the_third_party_identity() {
        let before = third_party_identity(None, Some("key-A"), None, Some("https://gw-a.example"));
        let after = third_party_identity(None, Some("key-A"), None, Some("https://gw-b.example"));
        assert_ne!(
            before, after,
            "a generic base_url edit over the same key is a change"
        );
    }

    /// The compare's final write is a compare-and-set, per half — a half whose
    /// entry a concurrent forget refreshed keeps that fresher value, never the
    /// stale identity the diff read.
    #[test]
    fn the_snapshot_write_keeps_a_concurrently_moved_entry() {
        use super::{CompareSkips, SnapshotIdentities, write_snapshot_half_cas};
        use std::collections::HashMap;

        let name = "a".to_string();
        let before = SnapshotIdentities {
            oauth: oa(None, Some("access-A")),
            third_party: None,
            codex: None,
            ..SnapshotIdentities::default()
        };
        let after = SnapshotIdentities {
            oauth: oa(None, Some("access-B")),
            third_party: None,
            codex: None,
            ..SnapshotIdentities::default()
        };
        let concurrent = SnapshotIdentities {
            oauth: oa(None, Some("access-C")),
            third_party: None,
            codex: None,
            ..SnapshotIdentities::default()
        };

        let mut snap: HashMap<String, SnapshotIdentities> = HashMap::new();
        snap.insert(name.clone(), concurrent.clone());

        write_snapshot_half_cas(&mut snap, &name, &before, &after, &CompareSkips::default());

        assert_eq!(
            snap.get(&name),
            Some(&concurrent),
            "a half whose entry moved since the diff keeps the concurrent writer's value"
        );
    }

    /// P1's snapshot-before-epoch order: `forget_memory_with_identity` refreshes
    /// the snapshot half BEFORE it bumps the epoch, so a partition reading both
    /// under the epoch guard never pairs a stale snapshot with the post-bump
    /// epoch. Pinned by source scan — the order is only observable through a
    /// concurrent partition in the window between the two writes, which no test
    /// can stage deterministically.
    #[test]
    fn the_snapshot_half_is_refreshed_before_the_epoch_bump() {
        let body = include_str!("forget.rs")
            .split_once("fn forget_memory_with_identity(")
            .expect("the forget body is defined")
            .1
            .split_once("fn clear_memory(")
            .expect("the forget body ends where clear_memory begins")
            .0;
        let refresh = body
            .find("refresh_snapshot_half(stores, name, half, refreshed);")
            .expect("the refresh runs in the forget body");
        let bump = body
            .find("bump_epoch(name, half);")
            .expect("the bump runs in the forget body");
        assert!(
            refresh < bump,
            "the snapshot half is refreshed BEFORE the epoch bump"
        );
    }
}
