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
//!   `~/.clauth/base-env.json`, which saves the user's levels and restores
//!   them when the account stops covering them.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::claude::BaseEnvEntry;
use crate::profile::{ModelSettings, Profile};

/// The values Claude Code's `modelSettings.<model>.effortLevel` schema takes.
pub(crate) const LEVELS: [&str; 4] = ["low", "medium", "high", "xhigh"];

/// The tier aliases a `[models]` row can pin to a model id.
const TIERS: [&str; 4] = ["fable", "opus", "sonnet", "haiku"];

/// Whether `value` is a level Claude Code keeps per model.
pub(crate) fn is_level(value: &str) -> bool {
    LEVELS.contains(&value)
}

pub(crate) const ROW_LEVELS_HINT: &str = "low, medium, high or xhigh";

pub(crate) fn invalid_row_level(per_model: bool, value: &str) -> bool {
    let value = value.trim();
    per_model && !value.is_empty() && !is_level(value)
}

pub(crate) fn invalid_selected_entry(profile: &Profile) -> bool {
    selected_model(&profile.models)
        .and_then(|model| selected_entries(profile, model).into_iter().next())
        .and_then(|key| profile.model_effort.get(&key))
        .is_some_and(|level| !is_level(level.trim()))
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
    let Some(model) = selected_model(&profile.models) else {
        return profile.effort.clone();
    };
    selected_entries(profile, model)
        .first()
        .and_then(|key| profile.model_effort.get(key))
        .cloned()
        .or_else(|| selected_level(profile))
        .or_else(|| profile.effort.clone())
}

pub(crate) fn selected_level(profile: &Profile) -> Option<String> {
    let model = selected_model(&profile.models)?;
    let key = settings_key(&profile.models, model);
    let mut levels = resolved(profile);
    levels
        .remove(&key)
        .or_else(|| tier_of(&key).and_then(|tier| levels.remove(tier)))
}

/// The `[model_effort]` keys that land on the same `modelSettings` key as
/// `model` (the alias and its `[models]` id, say), in [`entry_order`]: the
/// entries the effort row shows and replaces.
pub(crate) fn selected_entries(profile: &Profile, model: &str) -> Vec<String> {
    let target = settings_key(&profile.models, model);
    entry_order(&profile.models, &profile.model_effort)
        .into_iter()
        .filter(|key| settings_key(&profile.models, key) == target)
        .map(str::to_string)
        .collect()
}

/// The order in which `[model_effort]` keys claim their `modelSettings` key:
/// the selected model's own spelling first, then alphabetical order. The first
/// valid entry for a key wins, in [`resolved_parts`] and on the effort row
/// alike.
fn entry_order<'a>(
    models: &ModelSettings,
    model_effort: &'a BTreeMap<String, String>,
) -> Vec<&'a str> {
    let selected = selected_model(models);
    let mut keys: Vec<&str> = model_effort.keys().map(String::as_str).collect();
    keys.sort_by_key(|key| Some(*key) != selected);
    keys
}

/// Use a configured model id as written. Claude Code resolves aliases and
/// canonicalizes model ids when it reads settings.
pub(crate) fn settings_key(models: &ModelSettings, model: &str) -> String {
    let model = model.trim();
    let alias = model.to_ascii_lowercase();
    if TIERS.contains(&alias.as_str()) {
        return models
            .field(&alias)
            .map(|id| id.trim())
            .filter(|id| !id.is_empty())
            .unwrap_or(&alias)
            .to_owned();
    }
    model.to_owned()
}

fn tier_of(key: &str) -> Option<&str> {
    let name = key.strip_prefix("claude-")?;
    let (tier, _) = name.split_once('-')?;
    TIERS.contains(&tier).then_some(tier)
}

pub(crate) fn is_pinned(pinned: &std::collections::BTreeSet<String>, key: &str) -> bool {
    pinned.contains(key) || tier_of(key).is_some_and(|tier| pinned.contains(tier))
}

/// A user's exact tier key takes precedence over an alias in Claude Code.
/// Cover existing tier keys as well, preserving explicit exact-id entries.
pub(crate) fn expand_tier_levels(
    settings: &serde_json::Map<String, serde_json::Value>,
    levels: &mut BTreeMap<String, String>,
) {
    if let Some(models) = settings
        .get("modelSettings")
        .and_then(serde_json::Value::as_object)
    {
        for key in models.keys() {
            if let Some(level) = tier_of(key).and_then(|tier| levels.get(tier)).cloned() {
                levels.entry(key.clone()).or_insert(level);
            }
        }
    }
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
    for model in entry_order(models, model_effort) {
        let level = model_effort[model].trim();
        if model.trim().is_empty() || !is_level(level) {
            continue;
        }
        out.entry(settings_key(models, model))
            .or_insert_with(|| level.to_string());
    }
    // The scalar is the selected model's level only where no table entry for
    // that model landed.
    if let Some(selected) = selected_model(models)
        && let Some(level) = effort.map(str::trim)
        && is_level(level)
    {
        out.entry(settings_key(models, selected))
            .or_insert_with(|| level.to_string());
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
    path: Option<&std::path::Path>,
) -> Result<BTreeMap<String, String>> {
    if settings.contains_key("modelSettings") {
        let saved = crate::claude::saved_model_effort()?;
        for (id, entry) in saved {
            if level_of(settings, &id).is_some_and(|live| entry.wrote_live(live)) {
                set_level(settings, &id, entry.base)?;
            }
        }
    }
    let mut levels = levels.clone();
    expand_tier_levels(settings, &mut levels);
    write_levels(settings, &levels).with_context(|| {
        let path = path.map_or_else(|| "settings.json".into(), |path| path.display().to_string());
        format!("fix or remove modelSettings in {path}")
    })?;
    Ok(levels)
}

fn write_levels(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    levels: &BTreeMap<String, String>,
) -> Result<()> {
    for (id, level) in levels {
        set_level(settings, id, Some(level.as_str().into()))?;
    }
    Ok(())
}

/// Applied levels follow the runtime through adoption and Keychain retries.
#[derive(Default, serde::Deserialize, serde::Serialize)]
pub(crate) struct RuntimeLevels {
    current: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    previous: BTreeMap<String, String>,
}

impl RuntimeLevels {
    fn path(settings: &std::path::Path) -> std::path::PathBuf {
        settings.with_file_name(".clauth-model-effort.json")
    }

    pub(crate) fn read(
        path: &std::path::Path,
        settings: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<BTreeMap<String, String>> {
        let path = Self::path(path);
        let record: Self = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("failed to parse {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        Ok(record
            .previous
            .into_iter()
            .chain(record.current)
            .filter(|(id, level)| {
                level_of(settings, id).and_then(serde_json::Value::as_str) == Some(level)
            })
            .collect())
    }

    pub(crate) fn write(
        path: &std::path::Path,
        current: BTreeMap<String, String>,
        previous: BTreeMap<String, String>,
    ) -> Result<()> {
        let path = Self::path(path);
        if current.is_empty() && previous.is_empty() && !path.exists() {
            return Ok(());
        }
        let record = Self { current, previous };
        crate::profile::atomic_write_600(&path, serde_json::to_vec(&record)?)
            .with_context(|| format!("failed to write {}", path.display()))
    }
}

/// A swapped session's levels: a model the outgoing account pinned
/// (as recorded in the runtime) and the incoming one does not goes
/// back to what a fresh launch would write (`fresh`, the live file after
/// [`apply_runtime`]) while the session still holds the outgoing level; a level
/// the user changed or deleted in the session stays. The incoming account's
/// `levels` then land.
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
    write_levels(runtime, levels)
}

/// Update the same pre-write and landed maps that guard custom env values.
pub(crate) fn apply_live(
    settings: &mut serde_json::Map<String, serde_json::Value>,
    levels: &BTreeMap<String, String>,
    captured: &mut BTreeMap<String, BaseEnvEntry>,
    landed: &mut BTreeMap<String, BaseEnvEntry>,
) -> Result<()> {
    let mut levels = levels.clone();
    expand_tier_levels(settings, &mut levels);
    for (id, entry) in captured.iter() {
        if !levels.contains_key(id) {
            if level_of(settings, id).is_some_and(|live| entry.wrote_live(live)) {
                set_level(settings, id, entry.base.clone())?;
            }
            landed.remove(id);
        }
    }
    for (id, level) in &levels {
        let live = level_of(settings, id).cloned();
        let value = serde_json::Value::String(level.clone());
        let owned = captured
            .get(id)
            .filter(|entry| live.as_ref().is_some_and(|live| entry.wrote_live(live)));
        let base = owned.map_or_else(|| live.clone(), |entry| entry.base.clone());
        let (next, settled) =
            BaseEnvEntry::capture(base, &value, live.as_ref().filter(|_| owned.is_some()));
        captured.insert(id.clone(), next);
        landed.insert(id.clone(), settled);
        set_level(settings, id, Some(value))?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/inline/model_effort.rs"]
mod tests;
