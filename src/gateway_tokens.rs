//! The managed shunt gateway's client tokens: one per claude profile, minted
//! into clauth's own 0600 store (`~/.clauth/gateway-client-tokens.toml`) and
//! never shown. A token's shunt client name is its profile's name. The spawn
//! env ([`crate::gateway::gateway_env`]) joins the store's `name:token` pairs
//! onto the value shunt reads without clauth (the env file's, else the one the
//! gateway inherits) of the variable `[server.auth].tokens_env` names; shunt
//! reads that variable at spawn alone, so the daemon restarts a running
//! gateway when the store's bytes or the variable change. A profile delete
//! revokes its token and a rename moves it ([`plan_profile_delete`],
//! [`rename_profile_token`]).
//!
//! This module and the spawn env are the only readers of a stored value; the
//! add hands the token it minted back for the profile write.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the Services card's client-token rows, the next slices, are this engine's only callers"
    )
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::gateway::{
    ClientAuth, GatewayRecord, SecretToken, adopted_guard_client_auth, client_auth,
    guard_client_auth, mint_token,
};
use crate::gateway_edit::{Applied, Edit, apply_edit};
use crate::lock::{StateLockHeld, with_state_lock};
use crate::profile::{atomic_write_600, clauth_dir};

const STORE_FILE: &str = "gateway-client-tokens.toml";

/// The SHA-256 of the store's bytes.
pub(crate) type StoreDigest = [u8; 32];

pub(crate) fn store_path() -> Result<PathBuf> {
    Ok(clauth_dir()?.join(STORE_FILE))
}

/// The store's on-disk shape: profile name to token.
#[derive(Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

/// The store as one read saw it, with the digest of the bytes it came from.
/// `Debug` lists the profile names alone; no `Display`, no `PartialEq`.
#[derive(Default)]
pub(crate) struct ClientTokens {
    tokens: BTreeMap<String, String>,
    digest: Option<StoreDigest>,
}

impl std::fmt::Debug for ClientTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientTokens")
            .field("profiles", &self.names().collect::<Vec<_>>())
            .finish()
    }
}

impl ClientTokens {
    /// The store on disk; empty, with no digest, before the first add.
    pub(crate) fn load() -> Result<Self> {
        let path = store_path()?;
        let Some(bytes) = read_store(&path)? else {
            return Ok(Self::default());
        };
        // A parse error quotes the line it stopped at, which holds a token,
        // so no parser message rides the refusal.
        let file: StoreFile = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| toml::from_str(text).ok())
            .ok_or(ClientTokenRefusal::StoreUnreadable { path })?;
        Ok(Self {
            tokens: file.tokens,
            digest: Some(digest(&bytes)),
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The profiles holding a token, in name order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        self.tokens.keys().map(String::as_str)
    }

    /// The digest of the bytes this read came from; `None` with no store.
    pub(crate) fn digest(&self) -> Option<StoreDigest> {
        self.digest
    }

    /// The value the gateway's `variable` takes: the value shunt reads
    /// without clauth (`own`, with where it came from) first, then the
    /// store's pairs in name order, joined with `,`. Where shunt reads the
    /// variable (`strict`), a store entry shunt's grammar cannot carry
    /// refuses, and so do an `own` that does not parse and a store name
    /// `own` also holds, rather than hand shunt a value it refuses whole;
    /// elsewhere everything joins as it is, since shunt never reads it.
    pub(crate) fn spawn_value(
        &self,
        own: Option<(&str, &BaseSource)>,
        variable: &str,
        strict: bool,
    ) -> Result<String> {
        for (profile, token) in &self.tokens {
            if strict
                && (check_name(profile).is_err()
                    || token.is_empty()
                    || token.trim() != token
                    || token.contains(','))
            {
                return Err(ClientTokenRefusal::BadEntry {
                    profile: profile.clone(),
                    path: store_path()?,
                }
                .into());
            }
        }
        let pairs = self
            .tokens
            .iter()
            .map(|(profile, token)| format!("{profile}:{token}"))
            .collect::<Vec<_>>()
            .join(",");
        let Some((own, source)) = own else {
            return Ok(pairs);
        };
        if !strict {
            return Ok(if own.trim().is_empty() {
                pairs
            } else {
                format!("{own},{pairs}")
            });
        }
        let held = match parse_pairs(own) {
            TokensState::Unset => return Ok(pairs),
            TokensState::Unparsed => {
                return Err(ClientTokenRefusal::UnmanagedUnparsed {
                    op: None,
                    variable: variable.to_string(),
                    source: source.clone(),
                }
                .into());
            }
            TokensState::Names(held) => held,
        };
        let names: Vec<String> = self
            .names()
            .filter(|name| held.iter().any(|held| held == name))
            .map(str::to_string)
            .collect();
        if !names.is_empty() {
            return Err(ClientTokenRefusal::Collision {
                names,
                variable: variable.to_string(),
                source: source.clone(),
            }
            .into());
        }
        Ok(format!("{own},{pairs}"))
    }

    /// Whether `profile` holds a token.
    pub(crate) fn holds(&self, profile: &str) -> bool {
        self.tokens.contains_key(profile)
    }

    /// The one write path: under the state flock, load the store, hand the
    /// closure its entries, and save them only when they changed, so a no-op
    /// moves neither the bytes nor the digest the daemon restarts on.
    fn update<T>(f: impl FnOnce(&mut BTreeMap<String, String>) -> Result<T>) -> Result<T> {
        with_state_lock(|held| Self::update_held(held, f))
    }

    /// [`ClientTokens::update`] inside a hold the caller already took (a
    /// profile delete or rename), so the flock is never taken twice.
    fn update_held<T>(
        held: &StateLockHeld,
        f: impl FnOnce(&mut BTreeMap<String, String>) -> Result<T>,
    ) -> Result<T> {
        let before = Self::load()?;
        let mut tokens = before.tokens.clone();
        let out = f(&mut tokens)?;
        if tokens != before.tokens {
            save(tokens, held)?;
        }
        Ok(out)
    }
}

/// Persist, witness-gated; called by [`ClientTokens::update`] alone.
fn save(tokens: BTreeMap<String, String>, _held: &StateLockHeld) -> Result<()> {
    let path = store_path()?;
    let text = toml::to_string(&StoreFile { tokens })
        .context("failed to serialize the client-token store")?;
    atomic_write_600(&path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// The store's bytes; `None` with no store; a store that cannot be read
/// refuses typed ([`ClientTokenRefusal::StoreUnread`]).
fn read_store(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ClientTokenRefusal::StoreUnread {
            path: path.to_path_buf(),
            cause: e.to_string(),
        }
        .into()),
    }
}

fn digest(bytes: &[u8]) -> StoreDigest {
    <[u8; 32]>::from(Sha256::digest(bytes))
}

/// The digest of the store's bytes as they are now; `None` with no store.
pub(crate) fn store_digest() -> Result<Option<StoreDigest>> {
    Ok(read_store(&store_path()?)?.map(|bytes| digest(&bytes)))
}

/// What a tokens value holds, read as shunt reads it: `InboundAuthConfig::
/// resolve` takes a blank value as no pair, else `parse_tokens` (shunt
/// `8ed21d0` `src/config.rs`, `src/auth/inbound.rs`) splits it at `,`, trims
/// each entry and skips the empty ones, splits each at its first `:`, trims
/// both halves, and refuses the whole value on a colon-less entry, an empty
/// name or token, a duplicate name, or no entry left. Names only: the daemon
/// records this for its inherited value, never the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TokensState {
    /// Unset, blank, or not UTF-8, which shunt's `std::env::var` reads as
    /// unset: no pair.
    Unset,
    /// The client names, in order; never empty.
    Names(Vec<String>),
    /// A value shunt refuses whole, so it holds no pair clauth can count on
    /// and is never read as holding none.
    Unparsed,
}

/// [`TokensState`] of `value`.
pub(crate) fn parse_pairs(value: &str) -> TokensState {
    if value.trim().is_empty() {
        return TokensState::Unset;
    }
    let mut names: Vec<String> = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let Some((name, token)) = entry.split_once(':') else {
            return TokensState::Unparsed;
        };
        let (name, token) = (name.trim(), token.trim());
        if name.is_empty() || token.is_empty() || names.iter().any(|held| held == name) {
            return TokensState::Unparsed;
        }
        names.push(name.to_string());
    }
    if names.is_empty() {
        TokensState::Unparsed
    } else {
        TokensState::Names(names)
    }
}

/// A name shunt carries as a client name unchanged: not empty, no `:` (the
/// pair's split) or `,` (the entries'), and equal to its trimmed form, since
/// shunt trims it.
fn check_name(profile: &str) -> Result<(), ClientTokenRefusal> {
    if profile.trim().is_empty() || profile.trim() != profile || profile.contains([':', ',']) {
        return Err(ClientTokenRefusal::BadName {
            profile: profile.to_string(),
        });
    }
    Ok(())
}

/// Mint a client token for `profile` into the store, replacing any token it
/// held (the old one stops working when the gateway restarts on the change),
/// and hand it back for the profile's api key.
pub(crate) fn add_client_token(record: &GatewayRecord, profile: &str) -> Result<SecretToken> {
    check_name(profile)?;
    let op = format!("cannot add a client token for profile {profile:?}");
    let auth = client_auth(record).map_err(|e| for_op(&op, e))?;
    refuse_unmanaged_name(&auth, profile, None)?;
    let token = mint_token()?;
    ClientTokens::update(|tokens| {
        tokens.insert(profile.to_string(), token.expose().to_string());
        Ok(())
    })
    .map_err(|e| for_op(&op, e))?;
    Ok(token)
}

/// `e` as met by the op `op` names: a typed refusal wrapped, any other error
/// carried as its message, both opening on the op.
fn for_op(op: &str, e: anyhow::Error) -> anyhow::Error {
    match e.downcast::<ClientTokenRefusal>() {
        Ok(refusal) => ClientTokenRefusal::OpRefused {
            op: op.to_string(),
            refusal: Box::new(refusal),
        }
        .into(),
        Err(e) => ClientTokenRefusal::OpFailed {
            op: op.to_string(),
            cause: format!("{e:#}"),
        }
        .into(),
    }
}

/// A token named `name` would clash with a client the value shunt reads
/// without clauth already names, or that value does not parse, so no clash
/// can be ruled out; `renamed` is the profile a rename moves the token from.
fn refuse_unmanaged_name(auth: &ClientAuth, name: &str, renamed: Option<&str>) -> Result<()> {
    let op = || match renamed {
        Some(from) => format!("cannot rename profile {from:?} to {name:?}"),
        None => format!("cannot add a client token for profile {name:?}"),
    };
    match &auth.unmanaged {
        Some((TokensState::Names(held), source)) if held.iter().any(|held| held == name) => {
            Err(ClientTokenRefusal::Unmanaged {
                op: op(),
                name: name.to_string(),
                variable: auth.variable.clone(),
                source: source.clone(),
            }
            .into())
        }
        Some((TokensState::Unparsed, source)) => Err(ClientTokenRefusal::UnmanagedUnparsed {
            op: Some(op()),
            variable: auth.variable.clone(),
            source: source.clone(),
        }
        .into()),
        Some((TokensState::Names(_) | TokensState::Unset, _)) | None => Ok(()),
    }
}

/// The last-token guard: what removing the store's last token refuses with,
/// if anything. While the gateway requires a token, a value shunt reads
/// without clauth that holds no pair refuses with the last-token refusal (a
/// stopped gateway included, since its next start would refuse to run open),
/// and one that does not parse refuses naming it, since no remaining pair
/// can be counted on.
fn last_token_guard(auth: &ClientAuth) -> Option<ClientTokenRefusal> {
    if !auth.requires_tokens() {
        return None;
    }
    match &auth.unmanaged {
        Some((TokensState::Names(_), _)) => None,
        Some((TokensState::Unparsed, source)) => Some(ClientTokenRefusal::UnmanagedUnparsed {
            op: Some("cannot remove the last client token".to_string()),
            variable: auth.variable.clone(),
            source: source.clone(),
        }),
        Some((TokensState::Unset, _)) | None => Some(ClientTokenRefusal::LastToken),
    }
}

/// The store, read for `op`: a store that cannot be read or does not parse
/// refuses opening on the op, naming the file and the fix.
fn load_for(op: &str) -> Result<ClientTokens> {
    ClientTokens::load().map_err(|e| for_op(op, e))
}

/// The last-token guard for `op`, read from the gateway (`auth`, `None`
/// when the config does not exist); a read that failed refuses, opening
/// on the op and naming the fix, since it hides whether the gateway would be
/// left with no client token.
fn guard_from(op: &str, auth: Result<Option<ClientAuth>>) -> Result<Option<ClientTokenRefusal>> {
    match auth {
        Ok(Some(auth)) => Ok(last_token_guard(&auth)),
        Ok(None) => Ok(None),
        Err(e) => Err(ClientTokenRefusal::GatewayUnread {
            op: op.to_string(),
            what: "whether the gateway would be left with no client token",
            fix: "fix that, or add another client token first",
            cause: format!("{e:#}"),
        }
        .into()),
    }
}

/// Remove `profile`'s token; `false` when it held none. Refuses the last
/// token under [`last_token_guard`]; the gateway is read only for the last
/// one, outside the flock.
pub(crate) fn remove_client_token(record: &GatewayRecord, profile: &str) -> Result<bool> {
    let op = format!("cannot remove the client token of profile {profile:?}");
    let tokens = load_for(&op)?;
    let read = tokens.holds(profile) && tokens.tokens.len() == 1;
    let guard = if read {
        guard_from(&op, guard_client_auth(record))?
    } else {
        None
    };
    #[cfg(test)]
    run_pre_removal_hold_hook(profile);
    ClientTokens::update(|tokens| {
        if !tokens.contains_key(profile) {
            return Ok(false);
        }
        if tokens.len() == 1 {
            if !read {
                return Err(ClientTokenRefusal::Raced {
                    op,
                    what: "removal",
                }
                .into());
            }
            if let Some(refusal) = guard {
                return Err(refusal.into());
            }
        }
        tokens.remove(profile);
        Ok(true)
    })
}

/// A test's stand-in for another process changing the store between a
/// removal's read and its hold, run once for the profile it names.
#[cfg(test)]
pub(crate) type PreRemovalHoldHook = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
static PRE_REMOVAL_HOLD_HOOK: std::sync::Mutex<
    Option<std::collections::HashMap<String, PreRemovalHoldHook>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_pre_removal_hold_hook(profile: &str, f: PreRemovalHoldHook) {
    if let Ok(mut hooks) = PRE_REMOVAL_HOLD_HOOK.lock() {
        hooks
            .get_or_insert_with(std::collections::HashMap::new)
            .insert(profile.to_string(), f);
    }
}

#[cfg(test)]
fn run_pre_removal_hold_hook(profile: &str) {
    let hook = PRE_REMOVAL_HOLD_HOOK
        .lock()
        .ok()
        .and_then(|mut hooks| hooks.as_mut().and_then(|hooks| hooks.remove(profile)));
    if let Some(f) = hook {
        f();
    }
}

/// What a profile delete read of the gateway before taking the state flock,
/// so no config, env file or daemon record is read under it: the gateway is
/// read only when the profile's token was the store's last.
pub(crate) struct ProfileTokenPlan {
    read: bool,
    guard: Option<ClientTokenRefusal>,
}

/// Read before deleting `profile`, outside the flock: `None` when it holds
/// no token. Its token being the store's last, whether the gateway may lose
/// it ([`last_token_guard`]): with no adopted gateway, or an absent config,
/// nothing guards it; a read that hides the answer refuses.
pub(crate) fn plan_profile_delete(profile: &str) -> Result<Option<ProfileTokenPlan>> {
    let op = format!("cannot delete profile {profile:?}");
    let tokens = load_for(&op)?;
    if !tokens.holds(profile) {
        return Ok(None);
    }
    let read = tokens.tokens.len() == 1;
    let guard = if read {
        guard_from(&op, adopted_guard_client_auth())?
    } else {
        None
    };
    Ok(Some(ProfileTokenPlan { read, guard }))
}

/// Read before renaming `old` to `new`, outside the flock: a token moving to
/// a name the store already holds refuses, and so does one the value shunt
/// reads without clauth already uses, like an add. A profile holding no
/// token passes without a read of the gateway. Answers whether `old` held a
/// token, for [`rename_profile_token`] under the hold.
pub(crate) fn refuse_profile_rename(old: &str, new: &str) -> Result<bool> {
    let op = format!("cannot rename profile {old:?} to {new:?}");
    let tokens = load_for(&op)?;
    if !tokens.holds(old) {
        return Ok(false);
    }
    if tokens.holds(new) {
        return Err(ClientTokenRefusal::NameHeld {
            op,
            name: new.to_string(),
        }
        .into());
    }
    match adopted_guard_client_auth() {
        Ok(Some(auth)) => refuse_unmanaged_name(&auth, new, Some(old)).map(|()| true),
        Ok(None) => Ok(true),
        Err(e) => Err(ClientTokenRefusal::GatewayUnread {
            op,
            what: "whether the new name clashes with a client the gateway already names",
            fix: "fix that, then try again",
            cause: format!("{e:#}"),
        }
        .into()),
    }
}

/// Inside the delete's hold, before anything is removed: refuse when
/// `profile`'s token is the store's last and the plan guards it, so a refused
/// delete is a clean no-op. A token that became the last after a plan that
/// read nothing refuses, asking for a rerun, rather than read the gateway
/// under the flock.
pub(crate) fn refuse_profile_delete(
    plan: Option<&ProfileTokenPlan>,
    profile: &str,
    _held: &StateLockHeld,
) -> Result<()> {
    let Some(plan) = plan else {
        return Ok(());
    };
    let op = format!("cannot delete profile {profile:?}");
    let tokens = load_for(&op)?;
    if !tokens.holds(profile) || tokens.tokens.len() != 1 {
        return Ok(());
    }
    if !plan.read {
        return Err(ClientTokenRefusal::Raced { op, what: "delete" }.into());
    }
    match &plan.guard {
        Some(refusal) => Err(refusal.clone().into()),
        None => Ok(()),
    }
}

/// Inside the delete's hold: revoke `profile`'s token. The gateway restarts
/// once on the store's change.
pub(crate) fn revoke_profile_token(profile: &str, held: &StateLockHeld) -> Result<()> {
    ClientTokens::update_held(held, |tokens| {
        tokens.remove(profile);
        Ok(())
    })
}

/// Inside the rename's hold: move `old`'s token to `new`, the same key, so
/// the profile's api key keeps working once the gateway restarts on the
/// change. `planned` is [`refuse_profile_rename`]'s answer: whether `old`
/// held a token, and so had its new name checked, when the rename planned.
pub(crate) fn rename_profile_token(
    old: &str,
    new: &str,
    planned: bool,
    held: &StateLockHeld,
) -> Result<()> {
    ClientTokens::update_held(held, |tokens| {
        if !tokens.contains_key(old) {
            return Ok(());
        }
        // A token that appeared after a plan that read nothing was never
        // checked against the clients the gateway already names.
        if !planned {
            return Err(ClientTokenRefusal::RenameRaced {
                op: format!("cannot rename profile {old:?} to {new:?}"),
            }
            .into());
        }
        // Never overwrite: the token held under `new` would be lost.
        if tokens.contains_key(new) {
            return Err(ClientTokenRefusal::NameHeld {
                op: format!("cannot rename profile {old:?} to {new:?}"),
                name: new.to_string(),
            }
            .into());
        }
        if let Some(token) = tokens.remove(old) {
            tokens.insert(new.to_string(), token);
        }
        Ok(())
    })
}

/// Add `[server.auth]` to a config that has none, through the editor's
/// checked write ([`apply_edit`]), so `shunt check` runs under the spawn env
/// with the tokens in it. Refuses before any write while no client token
/// exists, since shunt refuses a `[server.auth]` with none; a config that
/// already has the table writes nothing.
pub(crate) fn require_client_tokens(record: &GatewayRecord) -> Result<Applied> {
    let op = "cannot require client tokens";
    let auth = client_auth(record).map_err(|e| for_op(op, e))?;
    if !auth.in_config && load_for(op)?.is_empty() {
        match &auth.unmanaged {
            Some((TokensState::Names(_), _)) => {}
            Some((TokensState::Unparsed, source)) => {
                return Err(ClientTokenRefusal::UnmanagedUnparsed {
                    op: Some(op.to_string()),
                    variable: auth.variable.clone(),
                    source: source.clone(),
                }
                .into());
            }
            Some((TokensState::Unset, _)) | None => {
                return Err(ClientTokenRefusal::NoToken.into());
            }
        }
    }
    apply_edit(record, &Edit::RequireClientTokens)
}

/// Why a client-token op or the spawn env refused. Names profiles, paths and
/// variable names, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClientTokenRefusal {
    /// A name shunt cannot carry as a client name.
    BadName { profile: String },
    /// The value shunt reads without clauth already holds a client by that
    /// name; `op` is the add's or the rename's `cannot …` phrase.
    Unmanaged {
        op: String,
        name: String,
        variable: String,
        source: BaseSource,
    },
    /// The value shunt reads without clauth does not parse; `op` is the
    /// refused op's `cannot …` phrase, `None` at the spawn.
    UnmanagedUnparsed {
        op: Option<String>,
        variable: String,
        source: BaseSource,
    },
    /// A failure that is no typed refusal, met by the op `op` names.
    OpFailed { op: String, cause: String },
    /// `refusal`, met by the op `op` names (`cannot delete profile …`).
    OpRefused {
        op: String,
        refusal: Box<ClientTokenRefusal>,
    },
    /// A gateway read `op` needs failed, hiding `what`; `fix` closes the
    /// message.
    GatewayUnread {
        op: String,
        what: &'static str,
        fix: &'static str,
        cause: String,
    },
    /// The token became the store's last after the op (`what`, a delete or a
    /// removal) read nothing of the gateway for it.
    Raced { op: String, what: &'static str },
    /// The store already holds a token under `name`.
    NameHeld { op: String, name: String },
    /// The renamed profile gained a token after the rename planned.
    RenameRaced { op: String },
    /// The require op, with no token in the store or the env file.
    NoToken,
    /// The remove of the last token while the gateway requires one.
    LastToken,
    /// At spawn: the value shunt reads without clauth and the store share
    /// client names.
    Collision {
        names: Vec<String>,
        variable: String,
        source: BaseSource,
    },
    /// At spawn: a hand-edited store entry shunt's grammar cannot carry.
    BadEntry { profile: String, path: PathBuf },
    /// The store does not parse.
    StoreUnreadable { path: PathBuf },
    /// The store cannot be read (`cause`, the io error).
    StoreUnread { path: PathBuf, cause: String },
}

impl std::fmt::Display for ClientTokenRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName { profile } => write!(
                f,
                "cannot add a client token for profile {profile:?}: shunt cannot carry that as a client name, which must not be empty, hold ':' or ',', or start or end with whitespace"
            ),
            Self::Unmanaged {
                op,
                name,
                variable,
                source,
            } => write!(
                f,
                "{op}: {} already names a client {name:?} in {variable}, and shunt refuses a duplicate name; remove that entry from {} first",
                source.named(),
                source.short()
            ),
            Self::UnmanagedUnparsed {
                op,
                variable,
                source,
            } => {
                if let Some(op) = op {
                    write!(f, "{op}: ")?;
                }
                write!(
                    f,
                    "{} holds a {variable} value shunt cannot parse, and shunt refuses to start on it; fix that value first",
                    source.named()
                )
            }
            Self::OpRefused { op, refusal } => write!(f, "{op}: {refusal}"),
            Self::OpFailed { op, cause } => write!(f, "{op}: {cause}"),
            Self::GatewayUnread {
                op,
                what,
                fix,
                cause,
            } => write!(f, "{op}: clauth cannot tell {what} ({cause}); {fix}"),
            Self::Raced { op, what } => write!(
                f,
                "{op}: its client token became the gateway's last while the {what} was being prepared; run the {what} again"
            ),
            Self::RenameRaced { op } => write!(
                f,
                "{op}: the profile gained a client token while the rename was being prepared; run the rename again"
            ),
            Self::NameHeld { op, name } => write!(
                f,
                "{op}: clauth's client-token store already holds a token for {name:?}; remove that token first"
            ),
            Self::NoToken => f.write_str(
                "cannot require client tokens: no client token exists yet; add one first",
            ),
            Self::LastToken => f.write_str(
                "cannot remove the last client token while the gateway requires tokens; add another token or remove [server.auth] from the config first",
            ),
            Self::Collision {
                names,
                variable,
                source,
            } => {
                let listed = names
                    .iter()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let (clients, entries, profiles) = if names.len() == 1 {
                    ("a client", "that entry", "that profile")
                } else {
                    ("clients", "those entries", "those profiles")
                };
                write!(
                    f,
                    "{} and clauth's client-token store both name {clients} {listed} in {variable}, and shunt refuses a duplicate name; remove {entries} from {}, or remove clauth's token for {profiles}",
                    source.named(),
                    source.short()
                )
            }
            Self::BadEntry { profile, path } => write!(
                f,
                "clauth's client-token store {} holds an entry for profile {profile:?} that shunt cannot read; remove that profile's token and add it again",
                path.display()
            ),
            Self::StoreUnread { path, cause } => write!(
                f,
                "clauth's client-token store {} cannot be read ({cause}); repair its permissions or remove it",
                path.display()
            ),
            Self::StoreUnreadable { path } => write!(
                f,
                "clauth's client-token store {} does not parse; delete it and add the tokens again",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ClientTokenRefusal {}

/// Where the value shunt reads without clauth came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BaseSource {
    /// The gateway's env file sets the variable.
    EnvFile(PathBuf),
    /// The env the gateway inherits: the daemon's.
    Inherited,
}

impl BaseSource {
    fn named(&self) -> String {
        match self {
            Self::EnvFile(path) => format!("the gateway's env file {}", path.display()),
            Self::Inherited => "the environment the gateway inherits".to_string(),
        }
    }

    fn short(&self) -> &'static str {
        match self {
            Self::EnvFile(_) => "the env file",
            Self::Inherited => "that environment",
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/gateway_tokens.rs"]
mod tests;
