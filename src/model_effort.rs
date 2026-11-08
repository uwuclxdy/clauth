//! Per-model reasoning effort: an account's levels for each model, written as
//! `modelSettings.<model>.effortLevel` in `settings.json`.
//!
//! Claude Code keeps one effort level per model under `modelSettings`, keyed by
//! the model's canonical name, and its `/effort` command writes there. An
//! account's levels come from two places on its profile:
//!
//! - `[model_effort]` in config.toml, keyed by a `model` row value (a tier
//!   alias such as `sonnet`, or a model id);
//! - the scalar `effort`, while the `model` row names one model
//!   ([`selected_model`]) and the scalar is a per-model level
//!   ([`is_level`]): it is then that model's level, unless the table already
//!   holds one for it.
//!
//! Claude Code's schema for the key takes `low`, `medium`, `high` and `xhigh`
//! only and drops any other value without a word, so [`resolved`] skips an
//! entry outside that set. The env carrier (`CLAUDE_CODE_EFFORT_LEVEL`)
//! outranks every per-model level, which is why
//! [`crate::claude::effective_effort`] writes it empty under a selected model.
//!
//! Two writes land the levels:
//!
//! - a session's runtime `settings.json` ([`apply_runtime`]), built from the
//!   live `~/.claude/settings.json`: the build first puts back every level the
//!   record says a switch wrote there for some account, so a session never
//!   inherits another account's levels;
//! - the live `~/.claude/settings.json` on a switch ([`apply_live`]), through
//!   `~/.clauth/model-effort.json`. Before covering a level the user set with
//!   `/effort`, the record saves it (`base`); a switch to an account that does
//!   not pin the model, or a switch-off, puts it back (or removes the key when
//!   there was none) unless the user changed the value meanwhile. The entry
//!   carries `pending` across the window between the record's pre-write and
//!   the live write, as `base-env.json`'s entries do, so a failed live write
//!   never turns the previous account's value into the user's.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::profile::{ModelSettings, Profile};

/// The values Claude Code's `modelSettings.<model>.effortLevel` schema takes.
pub(crate) const LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// The tier aliases a `[models]` row can pin to a model id.
const TIERS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// Whether `value` is a level Claude Code keeps per model.
pub(crate) fn is_level(value: &str) -> bool {
    LEVELS.contains(&value)
}

/// The one model an account's `model` row names, or `None` when it names
/// none: unset, `default`, or `opusplan` (opus in plan mode, sonnet
/// otherwise), which keep the account-wide effort.
pub(crate) fn selected_model(models: &ModelSettings) -> Option<&str> {
    let model = models.default.as_deref()?.trim();
    names_one_model(model).then_some(model)
}

/// Whether a `model` row value names one model ([`selected_model`]'s test on
/// a raw value, for a form that has no saved profile yet).
pub(crate) fn names_one_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    !model.is_empty() && model != "default" && model != "opusplan"
}

/// The level the Setup tab's `effort` row shows and edits: the selected
/// model's (its `[model_effort]` entry, else the scalar) while the `model` row
/// names one model, the account-wide scalar otherwise.
pub(crate) fn row_level(profile: &Profile) -> Option<String> {
    match selected_model(&profile.models) {
        Some(model) => selected_entries(profile, model)
            .first()
            .and_then(|key| profile.model_effort.get(key))
            .or(profile.effort.as_ref())
            .cloned(),
        None => profile.effort.clone(),
    }
}

/// The `[model_effort]` keys that land on the same `modelSettings` key as
/// `model` (the alias and its `[models]` id, say), `model`'s own spelling
/// first: the entries the effort row shows and replaces.
pub(crate) fn selected_entries(profile: &Profile, model: &str) -> Vec<String> {
    let target = settings_key(&profile.models, model);
    let mut keys: Vec<String> = profile
        .model_effort
        .keys()
        .filter(|key| settings_key(&profile.models, key) == target)
        .cloned()
        .collect();
    keys.sort_by_key(|key| key != model);
    keys
}

/// The `modelSettings` key clauth writes for a `model` value: a tier with a
/// `[models]` row resolves to that row's model id, a tier with none stays its
/// alias (Claude Code resolves an alias key to the tier's current model), and
/// any other value is a model id. Ids go through [`canonical_model_id`].
pub(crate) fn settings_key(models: &ModelSettings, model: &str) -> String {
    let model = model.trim();
    let lower = model.to_ascii_lowercase();
    if TIERS.contains(&lower.as_str()) {
        return match models.field(&lower).map(|id| id.trim()) {
            Some(id) if !id.is_empty() => canonical_model_id(id),
            _ => lower,
        };
    }
    canonical_model_id(model)
}

/// A Claude model id's provider and suffix spellings: an optional path, an
/// optional Bedrock `region.anthropic.` prefix, then the `claude-` name with an
/// optional `-YYYYMMDD` / `@YYYYMMDD` date and `-vN[:N]` version.
#[allow(clippy::expect_used, reason = "the model-id pattern is a literal")]
static CLAUDE_ID: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"^(?:.*/)?(?:[a-z-]+\.)?(?:anthropic\.)?(claude-[a-z0-9.-]*?)(?:[-@]\d{8})?(?:-v\d+(?::\d+)?)?(?:@\d{8})?$",
    )
    .expect("the model-id pattern is a literal")
});

/// The name Claude Code keys `modelSettings` by, for a model id. Claude Code
/// maps a Claude model's dated, `-vN`, `[1m]`/`[2m]`, Bedrock and Vertex
/// spellings to one canonical name, and within one settings file an exact
/// canonical key beats any other spelling of the same model, so a level
/// clauth writes under a raw `[models]` id would lose to the user's own
/// `/effort` entry. A dotted version (`claude-sonnet-4.5`) folds to its
/// dashed form too. Any other id (a gateway's own model) is left as written
/// apart from a trailing `[1m]`, as Claude Code leaves it; so is a moving
/// alias such as `claude-sonnet-4-5-latest`, which names no fixed model.
pub(crate) fn canonical_model_id(id: &str) -> String {
    let id = id.trim();
    let lower = id.to_ascii_lowercase();
    let bare = lower
        .strip_suffix("[1m]")
        .or_else(|| lower.strip_suffix("[2m]"))
        .unwrap_or(&lower);
    if let Some(caps) = CLAUDE_ID.captures(bare) {
        return dash_versions(&caps[1]);
    }
    match lower.strip_suffix("[1m]") {
        Some(_) => id[..id.len() - "[1m]".len()].to_string(),
        None => id.to_string(),
    }
}

/// `claude-sonnet-4.5` → `claude-sonnet-4-5`: a dot between two digits is a
/// version separator Claude Code spells with a dash.
fn dash_versions(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let between_digits = *c == '.'
                && i > 0
                && chars[i - 1].is_ascii_digit()
                && chars.get(i + 1).is_some_and(char::is_ascii_digit);
            if between_digits { '-' } else { *c }
        })
        .collect()
}

/// Whether a `modelSettings` key belongs to a model some account pins:
/// `pinned` holds [`resolved`] keys (canonical ids, or a tier alias without a
/// `[models]` row). An alias pin covers every Claude id of its tier, since
/// Claude Code's `/effort` writes the tier's current model under its
/// canonical id, which clauth cannot name.
pub(crate) fn is_pinned(pinned: &std::collections::BTreeSet<String>, key: &str) -> bool {
    let key = canonical_model_id(key);
    pinned.contains(&key)
        || TIERS
            .iter()
            .any(|tier| pinned.contains(*tier) && key.starts_with(&format!("claude-{tier}-")))
}

/// The account's per-model levels, keyed by the `modelSettings` key clauth
/// writes: the `[model_effort]` table, plus the scalar `effort` as the
/// selected model's level while the table holds none for it. An entry whose
/// value Claude Code would drop is skipped ([`invalid`] names them).
pub(crate) fn resolved(profile: &Profile) -> BTreeMap<String, String> {
    resolved_parts(
        &profile.models,
        profile.effort.as_deref(),
        &profile.model_effort,
    )
}

/// [`resolved`] over the three config.toml fields it reads, for a caller that
/// parses only those (the settings sync).
pub(crate) fn resolved_parts(
    models: &ModelSettings,
    effort: Option<&str>,
    model_effort: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(selected) = selected_model(models)
        && !model_effort.contains_key(selected)
        && let Some(level) = effort.map(str::trim)
        && is_level(level)
    {
        out.insert(settings_key(models, selected), level.to_string());
    }
    for (model, level) in model_effort {
        let level = level.trim();
        if model.trim().is_empty() || !is_level(level) {
            continue;
        }
        out.insert(settings_key(models, model), level.to_string());
    }
    out
}

/// The `[model_effort]` keys [`resolved`] skips because Claude Code would drop
/// their value.
pub(crate) fn invalid(profile: &Profile) -> Vec<&str> {
    profile
        .model_effort
        .iter()
        .filter(|(model, level)| !model.trim().is_empty() && !is_level(level.trim()))
        .map(|(model, _)| model.as_str())
        .collect()
}

/// One model's level as clauth covers it: the user's own `effortLevel` from
/// before the first cover (`base`, `None` when the key was absent), the level
/// clauth wrote (`wrote`), and the level clauth's live value still has while a
/// pre-write has already moved `wrote` on (`pending`), dropped once the live
/// write lands.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) base: Option<serde_json::Value>,
    pub(crate) wrote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pending: Option<String>,
}

/// A present `base` field, a JSON `null` included, is the user's value; only a
/// missing one reads as `None`.
fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<serde_json::Value>, D::Error> {
    serde::Deserialize::deserialize(deserializer).map(Some)
}

/// The levels a switch covers in the live `~/.claude/settings.json`, keyed by
/// `modelSettings` key, under a version stamp: `~/.clauth/model-effort.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    v: u32,
    #[serde(default)]
    pub(crate) ids: BTreeMap<String, Entry>,
}

const RECORD_VERSION: u32 = 1;

impl Default for Record {
    fn default() -> Self {
        Self {
            v: RECORD_VERSION,
            ids: BTreeMap::new(),
        }
    }
}

/// `~/.clauth/model-effort.json`, the record of the levels a switch wrote into
/// the live `~/.claude/settings.json`.
pub(crate) fn record_path() -> Result<PathBuf> {
    Ok(crate::profile::clauth_dir()?.join("model-effort.json"))
}

/// Read a record file, `None` when it does not exist. One that does not parse,
/// or names another version, is an error, never an empty record: an empty read
/// would give up the user's levels it holds.
pub(crate) fn read_record_at(path: &std::path::Path) -> Result<Option<Record>> {
    if !path.exists() {
        return Ok(None);
    }
    let context = || {
        format!(
            "{} holds the per-model effort levels clauth covers; repair it (removing it gives those levels up)",
            path.display()
        )
    };
    let record: Record = crate::profile::read_json_file(path).with_context(context)?;
    if record.v != RECORD_VERSION {
        return Err(crate::format::parse_error(path.display(), None)).with_context(context);
    }
    Ok(Some(record))
}

/// Write a record file (0o600; `~/.clauth` holds it under the state flock).
pub(crate) fn write_record_at(path: &std::path::Path, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(record).context("failed to serialize effort record")?;
    crate::profile::atomic_write_600(path, bytes)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Who a covered model's live `effortLevel` belongs to.
#[derive(Debug, PartialEq)]
enum Owner {
    /// clauth wrote it: the user's own level is the entry's `base`.
    Clauth(Option<serde_json::Value>),
    /// The user set or deleted it after clauth's write.
    User,
}

fn owner(entry: &Entry, live: Option<&serde_json::Value>) -> Owner {
    match live.and_then(serde_json::Value::as_str) {
        Some(level) if level == entry.wrote || entry.pending.as_deref() == Some(level) => {
            Owner::Clauth(entry.base.clone())
        }
        _ => Owner::User,
    }
}

/// The live `effortLevel` under `modelSettings.<id>`.
fn level_of<'a>(
    settings: &'a serde_json::Map<String, serde_json::Value>,
    id: &str,
) -> Option<&'a serde_json::Value> {
    settings
        .get("modelSettings")?
        .as_object()?
        .get(id)?
        .as_object()?
        .get("effortLevel")
}

/// Set (or, with `None`, remove) `modelSettings.<id>.effortLevel`, keeping the
/// model's other keys. A model object left empty is removed, and so is a
/// `modelSettings` left empty. A `modelSettings` or model entry that is not an
/// object fails cleanly.
fn set_level(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    id: &str,
    level: Option<serde_json::Value>,
) -> Result<()> {
    if level.is_none() && !settings.contains_key("modelSettings") {
        return Ok(());
    }
    let all = settings
        .entry("modelSettings")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("settings.json `modelSettings` is not an object")?;
    match level {
        Some(level) => {
            all.entry(id)
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .with_context(|| format!("settings.json `modelSettings.{id}` is not an object"))?
                .insert("effortLevel".into(), level);
        }
        None => {
            if let Some(model) = all.get_mut(id) {
                let model = model.as_object_mut().with_context(|| {
                    format!("settings.json `modelSettings.{id}` is not an object")
                })?;
                model.remove("effortLevel");
                if model.is_empty() {
                    all.remove(id);
                }
            }
        }
    }
    if all.is_empty() {
        settings.remove("modelSettings");
    }
    Ok(())
}

/// Land `levels` in a session's runtime `settings.json` object, built on the
/// live file. Every level the live record says clauth still owns there goes
/// back to its `base` first (the record is only read here, never retired), so
/// the session starts from the user's own levels; a level the user changed
/// since stays.
///
/// The record is read only when the base carries a `modelSettings` object,
/// the only case it can change the result; a record that does not parse then
/// fails the build rather than hand a session another account's levels.
pub(crate) fn apply_runtime(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    levels: &BTreeMap<String, String>,
) -> Result<()> {
    if settings.contains_key("modelSettings")
        && let Some(record) = read_record_at(&record_path()?)?
    {
        for (id, entry) in &record.ids {
            if let Owner::Clauth(base) = owner(entry, level_of(settings, id)) {
                set_level(settings, id, base)?;
            }
        }
    }
    for (id, level) in levels {
        set_level(settings, id, Some(level.as_str().into()))?;
    }
    Ok(())
}

/// The per-model half of a swap's effort handover: the incoming account's
/// levels and, when it could be read, the outgoing account's.
pub(crate) struct Handover<'a> {
    pub(crate) incoming: &'a BTreeMap<String, String>,
    pub(crate) outgoing: Option<&'a BTreeMap<String, String>>,
}

/// A swapped session's levels: a model the outgoing account pinned
/// (`outgoing`, its [`resolved`] levels) and the incoming one does not goes
/// back to what a fresh launch would write (`fresh`, the live file after
/// [`apply_runtime`]) while the session still holds the outgoing level; a level
/// the user changed or deleted in the session stays. The incoming account's
/// `levels` then land. `outgoing` is `None` when the outgoing account could not
/// be read: only the incoming levels land then.
pub(crate) fn hand_over(
    runtime: &mut serde_json::Map<String, serde_json::Value>,
    fresh: &serde_json::Map<String, serde_json::Value>,
    levels: &BTreeMap<String, String>,
    outgoing: Option<&BTreeMap<String, String>>,
) -> Result<()> {
    for (id, level) in outgoing.into_iter().flatten() {
        let held = level_of(runtime, id).and_then(serde_json::Value::as_str);
        if !levels.contains_key(id) && held == Some(level.as_str()) {
            set_level(runtime, id, level_of(fresh, id).cloned())?;
        }
    }
    for (id, level) in levels {
        set_level(runtime, id, Some(level.as_str().into()))?;
    }
    Ok(())
}

/// The record a live apply lands around its write: `found` as read (`None`
/// when absent), `captured` with every new or moved entry, `landed` with every
/// change.
pub(crate) struct LivePlan {
    found: Option<Record>,
    captured: Record,
    landed: Record,
}

/// Land `levels` (the incoming account's, empty on a switch-off) in the live
/// `~/.claude/settings.json` object and plan the record around it. A covered
/// model the incoming account does not pin goes back to its `base` while
/// clauth's level is still live; a level the user changed or deleted stays as
/// the user left it. Either way its entry retires once the live write lands.
/// A pinned model keeps its entry's `base` while clauth owns the live value,
/// and otherwise takes the live value (the user's) as its new `base`.
///
/// A record that does not parse fails the apply before anything is written.
pub(crate) fn apply_live(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    levels: &BTreeMap<String, String>,
) -> Result<LivePlan> {
    let found = read_record_at(&record_path()?)?;
    let record = found.clone().unwrap_or_default();
    let mut captured = record.clone();
    let mut landed = record.clone();
    for (id, entry) in &record.ids {
        if levels.contains_key(id) {
            continue;
        }
        if let Owner::Clauth(base) = owner(entry, level_of(settings, id)) {
            set_level(settings, id, base)?;
        }
        landed.ids.remove(id);
    }
    for (id, level) in levels {
        let live = level_of(settings, id).cloned();
        let held = record.ids.get(id);
        let (base, pending) = match held.map(|entry| owner(entry, live.as_ref())) {
            // The live value is clauth's (`wrote`, or a `pending` an earlier
            // failed takeover left): it stays recognisable until this write
            // lands, whichever account wrote it.
            Some(Owner::Clauth(base)) => {
                let pending = live
                    .as_ref()
                    .and_then(serde_json::Value::as_str)
                    .filter(|live| *live != level)
                    .map(str::to_string);
                (base, pending)
            }
            Some(Owner::User) | None => (live, None),
        };
        captured.ids.insert(
            id.clone(),
            Entry {
                base: base.clone(),
                wrote: level.clone(),
                pending,
            },
        );
        landed.ids.insert(
            id.clone(),
            Entry {
                base,
                wrote: level.clone(),
                pending: None,
            },
        );
        set_level(settings, id, Some(level.as_str().into()))?;
    }
    Ok(LivePlan {
        found,
        captured,
        landed,
    })
}

impl LivePlan {
    /// The pre-write, before the live file: every new or moved entry, so a
    /// crash before the live write still names clauth's values.
    pub(crate) fn land_before(&self) -> Result<()> {
        if self.found.as_ref() != Some(&self.captured)
            && (self.found.is_some() || !self.captured.ids.is_empty())
        {
            write_record_at(&record_path()?, &self.captured)?;
        }
        Ok(())
    }

    /// The post-write, after the live file landed: retirements and dropped
    /// `pending` digests. Best-effort, as the live write already landed.
    pub(crate) fn land_after(&self) {
        if self.landed == self.captured {
            return;
        }
        if let Err(e) = record_path().and_then(|path| write_record_at(&path, &self.landed)) {
            crate::logline::logline!("clauth: {e:#}");
        }
    }
}

#[cfg(test)]
#[path = "../tests/inline/model_effort.rs"]
mod tests;
