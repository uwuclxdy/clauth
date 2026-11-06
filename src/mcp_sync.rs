//! The MCP login sync's memory across passes, `~/.clauth/mcp-sync-state.json`:
//! which login LINEAGE every source holds per MCP server, and which lineages
//! were signed out.
//!
//! A lineage is one login's refresh chain, as a set of fingerprints known to
//! be that chain. It grows by two events only: the sync's own write of a copy
//! into a store ([`SyncState::note_written`]), and a source's own entry
//! changing between passes ([`SyncState::observe`]: Claude Code's refresh, or a
//! re-login in that session), unless that lineage was signed out of. So a
//! copy left holding a token its chain has
//! since rotated away still reads as a copy of that chain, never as an
//! independent login. With the record lost, torn or of another version every
//! entry is a first sight: identical fingerprints group, every other usable
//! entry is kept, and only gaps fill.
//!
//! A source that held a server on the previous pass and lacks it now, read
//! cleanly, signed out of it: the lineage is tombstoned, its copies are
//! removed from every store the sync may write, and a store that held it
//! takes for that server only a login the record did not know before the
//! sign-out, until it holds a usable copy of that server again (a signed-out
//! entry keeps it refusing). A re-login made in place over another account's
//! own login is that login's lineage, so it stays refused. A copy of the
//! signed-out login that changes in place (a source the sync does not write
//! still holds one: a re-login there, or a mere refresh) is a new login: it
//! stays and reaches every store, the signed-out ones included. The tombstone ends
//! once no source holds a copy of it on a pass that read every source; each
//! store's removal mark outlives it. A source that still reads but holds no
//! `mcpOAuth` object at all was rebuilt, never signed out: it is a gap again
//! and its record is cleared.
//!
//! A handed-off item's logins are lent to the fan-out unkeyed: they fold into
//! no source's row. One the item rotated since the daemon last read it, which
//! no keyed source holds yet, is a first sight there and starts a lineage of
//! its own, and the copies of the old one are never brought up to it: they
//! keep a copy that may be spent until Claude Code signs it out and the gap
//! fills, never a wrong login.
//!
//! The record holds no token: a fingerprint is a truncated SHA-256 of a token,
//! the server key is the `mcpOAuth` key. Written 0o600 under the state flock as
//! a read-modify-write that keeps a concurrent writer's observations.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

/// The record's file name under `~/.clauth`.
pub(crate) const STATE_FILE: &str = "mcp-sync-state.json";

/// The only version this build reads; any other reads as empty.
const VERSION: u32 = 1;

/// The credential-store key holding Claude Code's MCP-server logins.
const MCP_OAUTH_KEY: &str = "mcpOAuth";

/// The whole record. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncState {
    version: u32,
    #[serde(default)]
    lineages: BTreeMap<String, Lineage>,
    /// Per source key, per server: what that source held on the last pass.
    #[serde(default)]
    seen: BTreeMap<String, BTreeMap<String, Seen>>,
}

impl Default for SyncState {
    fn default() -> Self {
        Self {
            version: VERSION,
            lineages: BTreeMap::new(),
            seen: BTreeMap::new(),
        }
    }
}

/// One login's refresh chain on one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Lineage {
    server: String,
    fps: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tombstoned_at_ms: Option<u64>,
    /// For a tombstone: the other logins of its server the record knew before
    /// the pass that saw the sign-out. A store that signed out takes none of
    /// them, since each was there to take before; a login first seen in that
    /// pass or later is a new one.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    rivals: BTreeSet<String>,
}

/// What one source held for one server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Seen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lineage: Option<String>,
    /// `None` for a signed-out entry (neither token) and for a removal mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fp: Option<String>,
    /// The source holds no entry: it signed out of `lineage`, or the sync
    /// removed that lineage from it.
    #[serde(default, skip_serializing_if = "is_false")]
    removed: bool,
    /// For a removal mark: the logins of the server this store takes none
    /// of ([`Lineage::rivals`] when it was made). Kept after the tombstone
    /// ends, until the store holds the server again: the store signed out.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    refused: BTreeSet<String>,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// One source of a pass. `key` is `None` for a transient source (a handed-off
/// item's logins), which lends its entries and is never recorded. `value` is
/// `None` for a source that exists but did not read: it neither signs out nor
/// lets a tombstone end.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    pub(crate) key: Option<String>,
    pub(crate) value: Option<Value>,
}

/// The key of a credential file source: its canonical identity, so a live
/// link and the store it names are one source.
pub(crate) fn path_key(path: &Path) -> String {
    let id = crate::canonical::identity(path).unwrap_or_else(|_| path.to_path_buf());
    format!("path:{}", id.display())
}

/// The key of a live session's per-session Keychain item, by its tree.
#[cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, reason = "the only caller is the macOS item read")
)]
pub(crate) fn item_key(runtime: &Path) -> String {
    format!("item:{}", runtime.display())
}

/// An entry's fingerprint: the first 16 hex chars of a SHA-256 over its
/// refresh token, else over its access token; `None` for a signed-out entry.
pub(crate) fn fingerprint(entry: &Value) -> Option<String> {
    let token = ["refreshToken", "accessToken"]
        .iter()
        .find_map(|key| entry.get(*key)?.as_str().filter(|t| !t.is_empty()))?;
    Some(short_digest(token.as_bytes()))
}

fn short_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(&digest[..8])
}

fn entries(value: &Value) -> Option<&Map<String, Value>> {
    value.get(MCP_OAUTH_KEY)?.as_object()
}

fn usable(entry: &Value) -> bool {
    crate::claude::token_entry_usable(entry)
}

/// What a pass writes: per lineage, its server and the copy that expires
/// latest. Tombstoned lineages hold none.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    by_lineage: BTreeMap<String, (String, Value)>,
}

impl SyncState {
    /// The record on disk; absent, unreadable, torn or of another version
    /// reads as empty.
    pub(crate) fn load() -> Self {
        match state_path().map(|path| read_disk(&path)) {
            Ok(Disk::Read(state)) => state,
            Ok(Disk::Absent | Disk::Unreadable) | Err(_) => Self::default(),
        }
    }

    fn lineage_holding(&self, server: &str, fp: &str) -> Option<String> {
        self.lineages
            .iter()
            .find(|(_, l)| l.server == server && l.fps.contains(fp))
            .map(|(id, _)| id.clone())
    }

    fn tombstoned(&self, id: &str) -> bool {
        self.lineages
            .get(id)
            .is_some_and(|l| l.tombstoned_at_ms.is_some())
    }

    /// The lineage of `entry` for `server`: the one holding its fingerprint,
    /// else, for an entry `key`'s source held on the last pass, that one's
    /// (an in-place change the next observation folds in).
    fn lineage_for(&self, key: Option<&str>, server: &str, entry: &Value) -> Option<String> {
        if let Some(id) = fingerprint(entry).and_then(|fp| self.lineage_holding(server, &fp)) {
            return Some(id);
        }
        key.and_then(|k| self.seen.get(k))
            .and_then(|m| m.get(server))
            .filter(|s| !s.removed)
            .and_then(|s| s.lineage.clone())
    }

    fn entry_lineage(&self, server: &str, entry: &Value) -> Option<String> {
        fingerprint(entry).and_then(|fp| self.lineage_holding(server, &fp))
    }

    /// The lineage `fp` belongs to on `server`, minting one for a first sight
    /// or folding it into `prev`'s for an in-place change. A change made in
    /// place on a copy of a login signed out of is a new login: a refresh and
    /// a re-login look alike by fingerprint, and the re-login must survive.
    fn resolve(&mut self, server: &str, fp: Option<&str>, prev: Option<&Seen>) -> Option<String> {
        let Some(fp) = fp else {
            return prev.and_then(|p| p.lineage.clone());
        };
        if let Some(id) = self.lineage_holding(server, fp) {
            return Some(id);
        }
        let id = prev
            .and_then(|p| p.lineage.clone())
            .filter(|id| {
                self.lineages
                    .get(id)
                    .is_some_and(|l| l.server == server && l.tombstoned_at_ms.is_none())
            })
            .unwrap_or_else(|| format!("l-{}", short_digest(format!("{server}\0{fp}").as_bytes())));
        if !self.lineages.contains_key(&id) {
            self.forget_refusals_of(&id);
        }
        self.lineages
            .entry(id.clone())
            .or_insert_with(|| Lineage {
                server: server.to_string(),
                fps: BTreeSet::new(),
                tombstoned_at_ms: None,
                rivals: BTreeSet::new(),
            })
            .fps
            .insert(fp.to_string());
        Some(id)
    }

    /// Fold one pass's sources into the record: every entry's lineage, every
    /// vanish as a tombstone. `full` (the daemon's pass, which reads every
    /// source) also drops what no source holds any more. Returns the source
    /// keys whose record this pass rewrote.
    pub(crate) fn observe(
        &mut self,
        sources: &[Source],
        now_ms: u64,
        full: bool,
    ) -> BTreeSet<String> {
        let mut all_clean = true;
        let known_before: Vec<(String, String)> = self
            .lineages
            .iter()
            .map(|(id, l)| (id.clone(), l.server.clone()))
            .collect();
        let mut in_place: Vec<(usize, String, Option<String>, Option<Seen>)> = Vec::new();
        let mut first: Vec<(usize, String, Option<String>, Option<Seen>)> = Vec::new();
        // A live link and the store it names share one key: the first
        // occurrence speaks for both.
        let mut keys_read: BTreeSet<&String> = BTreeSet::new();
        let unique: Vec<(usize, &Source)> = sources
            .iter()
            .enumerate()
            .filter(|(_, src)| src.key.as_ref().is_none_or(|k| keys_read.insert(k)))
            .collect();
        for &(i, src) in &unique {
            let Some(value) = src.value.as_ref() else {
                all_clean = false;
                continue;
            };
            let Some(held) = entries(value) else {
                continue;
            };
            let prev = src.key.as_ref().and_then(|k| self.seen.get(k));
            for (server, entry) in held {
                let fp = fingerprint(entry);
                let prev_entry = prev
                    .and_then(|p| p.get(server))
                    .filter(|s| !s.removed)
                    .cloned();
                let row = (i, server.clone(), fp, prev_entry);
                if row.2.is_some() && row.3.is_some() {
                    in_place.push(row);
                } else {
                    first.push(row);
                }
            }
        }
        // Sign-outs first, so a change made in place on a copy of a login
        // signed out in this same pass is a new login as well.
        for &(_, src) in &unique {
            let (Some(key), Some(held)) = (src.key.as_ref(), src.value.as_ref().and_then(entries))
            else {
                continue;
            };
            let vanished: Vec<(String, String)> = self
                .seen
                .get(key)
                .into_iter()
                .flatten()
                .filter(|(server, s)| !s.removed && !held.contains_key(*server))
                .filter_map(|(server, s)| Some((server.clone(), s.lineage.clone()?)))
                .collect();
            for (server, id) in vanished {
                if let Some(lineage) = self.lineages.get_mut(&id)
                    && lineage.tombstoned_at_ms.is_none()
                {
                    lineage.tombstoned_at_ms = Some(now_ms);
                    lineage.rivals = known_before
                        .iter()
                        .filter(|(other, of)| *of == server && *other != id)
                        .map(|(other, _)| other.clone())
                        .collect();
                }
            }
        }
        let mut next: BTreeMap<String, BTreeMap<String, Seen>> = BTreeMap::new();
        // In-place changes first, so a rotated fingerprint joins its chain
        // before another source's first sight of it could mint a new one.
        for (i, server, fp, prev) in in_place.into_iter().chain(first) {
            let lineage = self.resolve(&server, fp.as_deref(), prev.as_ref());
            if let Some(key) = sources[i].key.clone() {
                next.entry(key).or_default().insert(
                    server,
                    Seen {
                        lineage,
                        fp,
                        removed: false,
                        refused: BTreeSet::new(),
                    },
                );
            }
        }
        let mut rewritten = BTreeSet::new();
        for &(_, src) in &unique {
            let (Some(key), Some(value)) = (src.key.as_ref(), src.value.as_ref()) else {
                continue;
            };
            // Claude Code's sign-out deletes one server's entry and keeps the
            // object; a file holding none at all was rebuilt (a fresh login
            // replaced it). Forgetting what it held makes a login later saved
            // there a first sight, never a rotation of the old one.
            let Some(held) = entries(value) else {
                if self.seen.remove(key).is_some() {
                    rewritten.insert(key.clone());
                }
                continue;
            };
            let mut now = next.remove(key).unwrap_or_default();
            for (server, prev) in self.seen.get(key).cloned().unwrap_or_default() {
                if let Some(entry) = held.get(&server) {
                    // A signed-out entry is no login: the store's sign-out
                    // stands, and so does what it refuses.
                    if prev.removed && !usable(entry) {
                        now.insert(server, prev);
                    }
                    continue;
                }
                if let Some(id) = prev.lineage {
                    let refused = if prev.removed {
                        prev.refused
                    } else {
                        self.rivals_of(&id)
                    };
                    now.insert(
                        server,
                        Seen {
                            lineage: Some(id),
                            fp: None,
                            removed: true,
                            refused,
                        },
                    );
                }
            }
            if self.seen.get(key) != Some(&now) {
                rewritten.insert(key.clone());
            }
            if now.is_empty() {
                self.seen.remove(key);
            } else {
                self.seen.insert(key.clone(), now);
            }
        }
        if full {
            let listed: BTreeSet<&String> = sources.iter().filter_map(|s| s.key.as_ref()).collect();
            let gone: Vec<String> = self
                .seen
                .keys()
                .filter(|k| !listed.contains(k))
                .cloned()
                .collect();
            for key in gone {
                self.seen.remove(&key);
                rewritten.insert(key);
            }
            self.prune(sources, all_clean);
        }
        rewritten
    }

    /// Keep each lineage's fingerprints some source holds now; drop a lineage
    /// nothing holds or names, a tombstoned one once nothing holds a copy of it
    /// on a pass that read every source. A removal mark keeps neither: it is
    /// the store's own sign-out, and goes when the store holds the server
    /// again, is rebuilt, or leaves the pass.
    fn prune(&mut self, sources: &[Source], all_clean: bool) {
        let mut held: BTreeSet<String> = self
            .seen
            .values()
            .flat_map(BTreeMap::values)
            .filter_map(|s| s.fp.clone())
            .collect();
        held.extend(
            sources
                .iter()
                .filter(|s| s.key.is_none())
                .filter_map(|s| s.value.as_ref().and_then(entries))
                .flat_map(|m| m.values().filter_map(fingerprint)),
        );
        let named: BTreeSet<String> = self
            .seen
            .values()
            .flat_map(BTreeMap::values)
            .filter(|s| !s.removed)
            .filter_map(|s| s.lineage.clone())
            .collect();
        self.lineages.retain(|id, lineage| {
            if lineage.tombstoned_at_ms.is_some() {
                // A tombstone keeps every fingerprint it covers: those are
                // what name a copy of the signed-out login wherever one turns
                // up. A source that did not read may hold one, which would
                // come back as a new login were the tombstone forgotten.
                return !all_clean || lineage.fps.iter().any(|fp| held.contains(fp));
            }
            lineage.fps.retain(|fp| held.contains(fp));
            !lineage.fps.is_empty() || named.contains(id)
        });
    }

    /// A lineage minted under `id` is a login the record had forgotten: an id
    /// derives from a server and a fingerprint, so a copy put back after its
    /// lineage was dropped mints the id it had. Every refusal naming the old
    /// lineage goes, so the copy reads as the new login it now is.
    fn forget_refusals_of(&mut self, id: &str) {
        for seen in self.seen.values_mut().flat_map(BTreeMap::values_mut) {
            seen.refused.remove(id);
        }
        for lineage in self.lineages.values_mut() {
            lineage.rivals.remove(id);
        }
    }

    /// The rivals of the tombstone `id` (none for any other lineage).
    fn rivals_of(&self, id: &str) -> BTreeSet<String> {
        self.lineages
            .get(id)
            .filter(|l| l.tombstoned_at_ms.is_some())
            .map(|l| l.rivals.clone())
            .unwrap_or_default()
    }

    /// The pass's plan over `sources` (after [`SyncState::observe`]).
    pub(crate) fn plan(&self, sources: &[Source]) -> Plan {
        let mut groups: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for value in sources.iter().filter_map(|s| s.value.as_ref()) {
            let Some(held) = entries(value) else {
                continue;
            };
            for (server, entry) in held {
                if !usable(entry) {
                    continue;
                }
                let Some(id) = self.entry_lineage(server, entry) else {
                    continue;
                };
                if self.tombstoned(&id) {
                    continue;
                }
                groups
                    .entry(id)
                    .or_default()
                    .push(serde_json::json!({ MCP_OAUTH_KEY: { server.clone(): entry.clone() } }));
            }
        }
        let mut plan = Plan::default();
        for (id, group) in groups {
            if let Some((server, winner)) =
                crate::claude::mcp_sync_winners(&group).into_iter().next()
            {
                plan.by_lineage.insert(id, (server, winner));
            }
        }
        plan
    }

    /// The latest-expiring of `plan`'s copies for `server`, none of a lineage
    /// `refused` names.
    fn newest_for(plan: &Plan, server: &str, refused: Option<&BTreeSet<String>>) -> Option<Value> {
        let candidates: Vec<Value> = plan
            .by_lineage
            .iter()
            .filter(|(id, (s, _))| s == server && refused.is_none_or(|r| !r.contains(*id)))
            .map(|(_, (s, winner))| serde_json::json!({ MCP_OAUTH_KEY: { s.clone(): winner.clone() } }))
            .collect();
        crate::claude::mcp_sync_winners(&candidates).remove(server)
    }

    /// Bring `target` (the store `key` names) up to `plan`, returning the
    /// servers changed. A copy of a tombstoned lineage is removed. A usable
    /// entry is replaced only by its own lineage's fresher copy; an absent or
    /// signed-out one takes the newest copy, except where the store signed
    /// out of that server (a removal mark, or a copy of a tombstoned lineage
    /// removed now): there no login the record knew before that sign-out
    /// lands ([`Lineage::rivals`]). Every other key of `target` is left as it
    /// was. PURE.
    pub(crate) fn apply(&self, key: Option<&str>, target: &mut Value, plan: &Plan) -> Vec<String> {
        let Some(obj) = target.as_object_mut() else {
            return Vec::new();
        };
        let mut held = match obj.get(MCP_OAUTH_KEY) {
            None => Map::new(),
            Some(Value::Object(held)) => held.clone(),
            Some(_) => return Vec::new(),
        };
        let mut removed: BTreeMap<String, String> = BTreeMap::new();
        held.retain(|server, entry| {
            let Some(id) = self
                .entry_lineage(server, entry)
                .filter(|id| self.tombstoned(id))
            else {
                return true;
            };
            removed.insert(server.clone(), id);
            false
        });
        let marks = key.and_then(|k| self.seen.get(k));
        let servers: BTreeSet<&String> = plan.by_lineage.values().map(|(s, _)| s).collect();
        let mut chosen = Map::new();
        for server in servers {
            match held.get(server) {
                Some(entry) if usable(entry) => {
                    if let Some(id) = self.lineage_for(key, server, entry)
                        && let Some((_, winner)) = plan.by_lineage.get(&id)
                    {
                        chosen.insert(server.clone(), winner.clone());
                    }
                }
                _ => {
                    let refused = match removed.get(server) {
                        Some(id) => Some(self.rivals_of(id)),
                        None => marks
                            .and_then(|m| m.get(server))
                            .filter(|s| s.removed)
                            .map(|s| s.refused.clone()),
                    };
                    if let Some(winner) = Self::newest_for(plan, server, refused.as_ref()) {
                        chosen.insert(server.clone(), winner);
                    }
                }
            }
        }
        if !removed.is_empty() {
            obj.insert(MCP_OAUTH_KEY.to_string(), Value::Object(held));
        }
        let mut changed = crate::claude::mcp_sync_apply(target, &chosen);
        for server in removed.into_keys() {
            if !changed.contains(&server) {
                changed.push(server);
            }
        }
        changed
    }

    /// Record that `target` (the store `key` names) now holds what the sync
    /// wrote for `changed`: a copy's lineage, or a removal mark.
    pub(crate) fn note_written(&mut self, key: &str, target: &Value, changed: &[String]) {
        let held = entries(target);
        let rows: Vec<(String, Seen)> = changed
            .iter()
            .map(|server| {
                let seen = match held.and_then(|m| m.get(server)) {
                    Some(entry) => Seen {
                        lineage: self.entry_lineage(server, entry),
                        fp: fingerprint(entry),
                        removed: false,
                        refused: BTreeSet::new(),
                    },
                    None => {
                        let lineage = self
                            .seen
                            .get(key)
                            .and_then(|m| m.get(server))
                            .and_then(|s| s.lineage.clone());
                        Seen {
                            refused: lineage
                                .as_deref()
                                .map(|id| self.rivals_of(id))
                                .unwrap_or_default(),
                            lineage,
                            fp: None,
                            removed: true,
                        }
                    }
                };
                (server.clone(), seen)
            })
            .collect();
        self.seen.entry(key.to_string()).or_default().extend(rows);
    }

    /// The switch carry's same-login rule over `merged` (the target after
    /// the carry), against `own` (the target before it): an own usable login
    /// the record knows, which the carry replaced with a copy the record knows
    /// as another login, or dropped, goes back. A copy of the same login stays
    /// carried, and so does any copy the record cannot place (the carry's
    /// behaviour with the sync off); a tombstoned own login is not put back.
    /// Returns whether `merged` changed. PURE.
    pub(crate) fn keep_own_logins(
        &self,
        merged: &mut Value,
        own: &Value,
        live_key: Option<&str>,
        own_key: Option<&str>,
    ) -> bool {
        let Some(own_held) = entries(own) else {
            return false;
        };
        let Some(obj) = merged.as_object_mut() else {
            return false;
        };
        let mut out = match obj.get(MCP_OAUTH_KEY) {
            None => Map::new(),
            Some(Value::Object(held)) => held.clone(),
            Some(_) => return false,
        };
        let mut changed = false;
        for (server, mine) in own_held {
            if !usable(mine) {
                continue;
            }
            let my_lineage = self.lineage_for(own_key, server, mine);
            if my_lineage.as_ref().is_some_and(|id| self.tombstoned(id)) {
                continue;
            }
            // Only a login the record knows as another one is kept apart: an
            // unknown copy may be a rotation made since the last pass, which
            // the carry exists to bring over.
            let Some(my_lineage) = my_lineage else {
                continue;
            };
            let keep_mine = match out.get(server) {
                Some(carried) if carried == mine => false,
                Some(carried) => self
                    .lineage_for(live_key, server, carried)
                    .is_some_and(|theirs| theirs != my_lineage),
                None => true,
            };
            if keep_mine {
                out.insert(server.clone(), mine.clone());
                changed = true;
            }
        }
        if changed {
            obj.insert(MCP_OAUTH_KEY.to_string(), Value::Object(out));
        }
        changed
    }

    /// The switch carry's whole rule over `merged` (the target after the
    /// carry), against `own` (the target before it): an own login goes back
    /// ([`SyncState::keep_own_logins`]), then no copy of a signed-out login
    /// stays, whichever side it came from. Returns whether `merged` changed.
    /// PURE.
    pub(crate) fn carry_rule(
        &self,
        merged: &mut Value,
        own: &Value,
        live_key: Option<&str>,
        own_key: Option<&str>,
    ) -> bool {
        let kept = self.keep_own_logins(merged, own, live_key, own_key);
        let before = merged.get(MCP_OAUTH_KEY).cloned();
        if let Some(obj) = merged.as_object_mut() {
            self.drop_tombstoned(obj);
        }
        kept || merged.get(MCP_OAUTH_KEY) != before.as_ref()
    }

    /// Drop every copy of a tombstoned lineage from `carried` (a handed-off
    /// item's carried keys); an `mcpOAuth` left empty goes too. PURE.
    pub(crate) fn drop_tombstoned(&self, carried: &mut Map<String, Value>) {
        if let Some(Value::Object(held)) = carried.get_mut(MCP_OAUTH_KEY) {
            held.retain(|server, entry| {
                !self
                    .entry_lineage(server, entry)
                    .is_some_and(|id| self.tombstoned(&id))
            });
            if held.is_empty() {
                carried.remove(MCP_OAUTH_KEY);
            }
        }
    }

    /// `self` merged over `theirs` (the record on disk now), where `loaded`
    /// is what this pass started from and `rewritten` the source keys it
    /// observed or wrote: a concurrent writer's observation of any other key,
    /// and every fingerprint or tombstone it added, are kept.
    fn merged_over(&self, loaded: &Self, theirs: &Self, rewritten: &BTreeSet<String>) -> Self {
        let mut out = self.clone();
        for (key, by_server) in &theirs.seen {
            if !rewritten.contains(key) && loaded.seen.get(key) != Some(by_server) {
                out.seen.insert(key.clone(), by_server.clone());
            }
        }
        for (id, lineage) in &theirs.lineages {
            let base = loaded.lineages.get(id);
            let added: Vec<String> = lineage
                .fps
                .iter()
                .filter(|fp| base.is_none_or(|b| !b.fps.contains(*fp)))
                .cloned()
                .collect();
            let tombstoned = lineage.tombstoned_at_ms.is_some()
                && base.is_none_or(|b| b.tombstoned_at_ms.is_none());
            if added.is_empty() && !tombstoned {
                continue;
            }
            // A lineage this pass dropped comes back as the other writer left
            // it, a tombstone included: a copy it saw means the end was early.
            let entry = out.lineages.entry(id.clone()).or_insert_with(|| Lineage {
                server: lineage.server.clone(),
                fps: BTreeSet::new(),
                tombstoned_at_ms: lineage.tombstoned_at_ms,
                rivals: lineage.rivals.clone(),
            });
            entry.fps.extend(added);
            if tombstoned && entry.tombstoned_at_ms.is_none() {
                entry.tombstoned_at_ms = lineage.tombstoned_at_ms;
                entry.rivals.clone_from(&lineage.rivals);
            }
        }
        out
    }
}

/// `~/.clauth/mcp-sync-state.json`.
pub(crate) fn state_path() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join(STATE_FILE))
}

enum Disk {
    Absent,
    Unreadable,
    Read(SyncState),
}

fn read_disk(path: &Path) -> Disk {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Disk::Absent,
        Err(_) => return Disk::Unreadable,
    };
    match serde_json::from_slice::<SyncState>(&bytes) {
        Ok(state) if state.version == VERSION => Disk::Read(state),
        Ok(_) | Err(_) => Disk::Unreadable,
    }
}

/// Write `ours` back over the record on disk, under the state flock, keeping
/// what a concurrent writer added since `loaded` ([`SyncState::merged_over`]).
/// Writes nothing when this pass changed nothing, and nothing when the merge
/// reproduces the file. A file that does not read is moved aside first
/// (`<name>.corrupt-<epoch_ms>`, 0o600), never written over.
///
/// # Errors
/// The flock, the record path or the write failed.
pub(crate) fn write_back(
    loaded: &SyncState,
    ours: &SyncState,
    rewritten: &BTreeSet<String>,
) -> Result<()> {
    if ours == loaded {
        return Ok(());
    }
    crate::lock::with_state_lock(|_held| {
        let path = state_path()?;
        let theirs = match read_disk(&path) {
            Disk::Read(state) => state,
            Disk::Absent => SyncState::default(),
            Disk::Unreadable => {
                if let Some(dir) = path.parent() {
                    crate::tps::move_aside(dir, STATE_FILE);
                }
                SyncState::default()
            }
        };
        let merged = ours.merged_over(loaded, &theirs, rewritten);
        if merged == theirs {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&merged)
            .context("failed to serialize the MCP sync record")?;
        crate::profile::atomic_write_600(&path, bytes)
            .with_context(|| format!("failed to write {}", path.display()))
    })
}

#[cfg(test)]
#[path = "../tests/inline/mcp_sync.rs"]
mod tests;
