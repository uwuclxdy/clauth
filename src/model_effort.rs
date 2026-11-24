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
//! entry outside that set. The account-wide carrier
//! (`CLAUDE_CODE_EFFORT_LEVEL`) outranks every per-model level, so
//! [`crate::claude::effective_effort`] writes it empty while the selected
//! model's `effort` is a level; a value the carrier's parser takes and the
//! schema cannot hold (`max`, `med`, a token budget) keeps the carrier, and
//! the `auto`/`unset` sentinels run the model default, capped by any settings
//! `maxEffortLevel` ([`selected_effort`] mirrors the resolver).
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

/// Whether the effort row shows a `[model_effort]` entry Claude Code drops
/// (never a value the carrier supplies, which applies ahead of the table).
pub(crate) fn invalid_selected_entry(profile: &Profile) -> bool {
    selected_effort(profile).is_some_and(|selected| {
        profile
            .model_effort
            .get(&selected.entry)
            .is_some_and(|level| {
                !is_level(level.trim()) && selected.shown.as_deref() == Some(level.as_str())
            })
    })
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

/// The level the Setup tab's `effort` row shows: [`selected_effort`]'s while
/// the `model` row names one model, the account-wide scalar otherwise.
pub(crate) fn row_level(profile: &Profile) -> Option<String> {
    match selected_effort(profile) {
        Some(selected) => selected.shown,
        None => profile.effort.clone(),
    }
}

/// The selected model's effort as a fresh launch applies it, which the Setup
/// row shows and edits and a moved session's carrier hands over.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SelectedEffort {
    /// The `[model_effort]` key an edit writes, and a clear removes while the
    /// row shows it: the entry whose level a launch applies, else the first
    /// entry for the model Claude Code drops, else the `model` row's own
    /// spelling. While the row shows a carrier value (the account's `effort` or
    /// an `[env]` entry), an edit still writes this entry and a clear leaves it.
    pub(crate) entry: String,
    /// Whether `shown` is a carrier value (the account's `effort` or an `[env]`
    /// entry) instead of an entry: it rides the carrier, so a clear removes the
    /// account's `effort` alone, leaving the entry it overrode and the `[env]`
    /// entry in place.
    pub(crate) account_wide: bool,
    /// The value the row shows: the carrier's value while Claude Code's parser
    /// takes it (`auto` for the `auto`/`unset` sentinels), else the per-model
    /// level that applies, else the entry Claude Code drops, else nothing.
    pub(crate) shown: Option<String>,
    /// The value a moved session's carrier hands over: `shown` wherever that
    /// makes the session apply what a fresh launch applies, and the per-model
    /// level (`None` when nothing does) where the row shows a dropped entry.
    pub(crate) level: Option<String>,
}

/// Which of the account's levels a fresh launch applies to the selected
/// model, read the way Claude Code reads the built file: the carrier's value
/// while its parser takes it ([`carrier_class`]), else the per-model table's
/// level, where of the written keys naming that model an exact canonical one
/// wins, else the first in the account's spelling order ([`resolved`]'s). A
/// key a member's file already holds keeps that file's position, so an
/// existing spelling outside this order can take the level instead. A tier
/// alias with no `[models]` row runs a model clauth cannot see, so only the
/// alias's own entry is its level. `None` while the `model` row names no one
/// model.
pub(crate) fn selected_effort(profile: &Profile) -> Option<SelectedEffort> {
    let model = selected_model(&profile.models)?;
    let lands = lands_on(settings_key(&profile.models, model));
    let entries = selected_entries(profile, model);
    let valid = |key: &&String| {
        profile
            .model_effort
            .get(*key)
            .is_some_and(|level| is_level(level.trim()))
    };
    let applied = resolved(profile)
        .into_iter()
        .filter(|(written, _)| lands(written))
        .min_by_key(|(written, _)| *written != canonical_name(written));
    let applied_entry = applied.as_ref().and_then(|(written, _)| {
        entries
            .iter()
            .filter(valid)
            .find(|key| settings_key(&profile.models, key) == *written)
            .cloned()
    });
    let applied_level = applied.map(|(_, level)| level);
    let shown_entry = applied_entry.or_else(|| entries.iter().find(|key| !valid(key)).cloned());
    let (entry, table_shown) = match shown_entry {
        Some(entry) => {
            let shown = profile.model_effort.get(&entry).cloned();
            (entry, shown)
        }
        None => (model.to_owned(), applied_level.clone()),
    };
    let carrier = crate::claude::effective_effort(profile);
    let (account_wide, shown, level) = match carrier_class(&carrier) {
        Carrier::Taken => (true, Some(carrier.clone()), Some(carrier)),
        Carrier::Sentinels => (true, Some("auto".to_string()), Some("auto".to_string())),
        Carrier::Dropped => (false, table_shown, applied_level),
    };
    Some(SelectedEffort {
        entry,
        account_wide,
        shown,
        level,
    })
}

/// How Claude Code's `CLAUDE_CODE_EFFORT_LEVEL` parser reads a carrier value
/// (bundle 2.1.330, `function hZ` at 315943041, `function nk` at 315943481).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Carrier {
    /// A value it takes as an effort: a level (`max` included), `med`, or a
    /// token budget.
    Taken,
    /// The `auto`/`unset` sentinels: the model default runs, capped by any
    /// settings `maxEffortLevel`, never a per-model level.
    Sentinels,
    /// Anything else, the empty string included: the parser drops it, so the
    /// per-model level applies.
    Dropped,
}

fn carrier_class(value: &str) -> Carrier {
    let lower = value.to_lowercase();
    if lower == "auto" || lower == "unset" {
        Carrier::Sentinels
    } else if rides_carrier(value) {
        Carrier::Taken
    } else {
        Carrier::Dropped
    }
}

/// Whether Claude Code's `CLAUDE_CODE_EFFORT_LEVEL` parser takes `value` as an
/// effort: a level, `max`, the `med` spelling of `medium`, or a token budget
/// (a value `parseInt` reads a leading integer from). Any other value leaves
/// the per-model level in force; the `auto`/`unset` sentinels go to
/// [`Carrier::Sentinels`].
fn rides_carrier(value: &str) -> bool {
    let value = value.to_lowercase();
    match value.as_str() {
        "med" => true,
        value => LEVELS.contains(&value) || value == "max" || numeric_prefix(value),
    }
}

/// Whether a leading integer spells `value`: `parseInt`'s leading whitespace,
/// an optional sign, then a digit, the shape Claude Code parses as a token
/// budget.
fn numeric_prefix(value: &str) -> bool {
    let bytes = value.trim_start_matches(parse_int_space).as_bytes();
    match bytes.first() {
        Some(b'+' | b'-') => bytes.get(1).is_some_and(u8::is_ascii_digit),
        Some(first) => first.is_ascii_digit(),
        None => false,
    }
}

/// The whitespace `parseInt` skips before a number: the ASCII set (vertical
/// tab included), `U+00A0`, `U+FEFF`, `U+1680`, `U+2000`-`U+200A`, `U+2028`,
/// `U+2029`, `U+202F`, `U+205F` and `U+3000`. `U+0085` is no whitespace to it,
/// where `char::is_whitespace` takes it.
fn parse_int_space(c: char) -> bool {
    c.is_ascii_whitespace()
        || c == '\u{b}'
        || c == '\u{a0}'
        || c == '\u{feff}'
        || c == '\u{1680}'
        || ('\u{2000}'..='\u{200a}').contains(&c)
        || c == '\u{2028}'
        || c == '\u{2029}'
        || c == '\u{202f}'
        || c == '\u{205f}'
        || c == '\u{3000}'
}

/// The `[model_effort]` keys whose level lands on `model`, ranked the way
/// [`selected_effort`] reads Claude Code's lookup: an exact canonical written
/// key first, then the account's own key order; a key a member's file already
/// holds keeps that file's position, so the file's order can differ. Keys
/// sharing one written key rank by [`entry_order`].
pub(crate) fn selected_entries(profile: &Profile, model: &str) -> Vec<String> {
    let lands = lands_on(settings_key(&profile.models, model));
    let mut entries: Vec<(bool, String, &str)> =
        entry_order(&profile.models, &profile.model_effort)
            .into_iter()
            .filter_map(|key| {
                let written = settings_key(&profile.models, key);
                lands(&written).then(|| (written != canonical_name(&written), written, key))
            })
            .collect();
    entries.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    entries
        .into_iter()
        .map(|(_, _, key)| key.to_owned())
        .collect()
}

/// Whether a written `modelSettings` key lands on the model `target` (a
/// [`settings_key`]) runs. A bare tier alias resolves through the reading
/// session's own alias table, so only that alias lands on it.
fn lands_on(target: String) -> impl Fn(&str) -> bool {
    let canonical = (!TIERS.contains(&target.as_str())).then(|| canonical_name(&target));
    move |written| match &canonical {
        None => written == target,
        Some(canonical) => !TIERS.contains(&written) && canonical_name(written) == *canonical,
    }
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

/// The tier a canonical Claude id names, in either shape the catalog spells:
/// `claude-<tier>-...` or the older `claude-<major>[-<minor>]-<tier>`.
fn tier_of_id(canonical: &str) -> Option<&str> {
    if let Some(tier) = tier_of(canonical) {
        return Some(tier);
    }
    let name = canonical.strip_prefix("claude-")?;
    let (major, rest) = name.split_once('-')?;
    if !(1..=2).contains(&major.len()) || !major.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let family = match rest.split_once('-') {
        Some((minor, family))
            if (1..=2).contains(&minor.len()) && minor.bytes().all(|b| b.is_ascii_digit()) =>
        {
            family
        }
        Some(_) => return None,
        None => rest,
    };
    TIERS.contains(&family).then_some(family)
}

/// The region prefixes Claude Code accepts before `anthropic.` in a model id.
const REGIONS: [&str; 7] = ["us", "eu", "apac", "jp", "au", "us-gov", "global"];

/// A `.` between a Claude version's numbers, which Claude Code reads as `-`.
#[expect(clippy::expect_used, reason = "the version-dot pattern is a literal")]
static VERSION_DOT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(claude-(?:[a-z]+-)?[0-9]{1,2})\.([0-9])")
        .expect("the version-dot pattern is a literal")
});

/// What may follow a Claude release's version for the id to still name that
/// release: a `-fast` or `-latest` suffix, a `-vN` revision, a date
/// (`-20251101`, `@20251101`).
#[expect(
    clippy::expect_used,
    reason = "the release-suffix pattern is a literal"
)]
static RELEASE_SUFFIX: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"^(?:-fast|-latest)?(?:-v[0-9]{1,3}@[0-9]{8}|[-@][0-9]{8})?(?:-v[0-9]{1,3}(?::[0-9]{1,3})?)?$",
    )
    .expect("the release-suffix pattern is a literal")
});

/// The name Claude Code keys a model's `modelSettings` entry by: lowercase,
/// a trailing `[1m]` gone, and a Claude release id (dated, `@date`, `-vN`,
/// `<region>.anthropic.`, a dotted version) folded to its release name. Any
/// other id keeps its spelling, less a trailing `-YYYYMMDD`. A tier alias
/// stays an alias: Claude Code resolves it per session.
pub(crate) fn canonical_name(id: &str) -> String {
    let id = VERSION_DOT
        .replace_all(&id.trim().to_ascii_lowercase(), "${1}-${2}")
        .into_owned();
    let folded = claude_release(&id).unwrap_or_else(|| {
        id.rsplit_once('-')
            .filter(|(_, date)| date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()))
            .map_or_else(|| id.clone(), |(name, _)| name.to_owned())
    });
    match folded.strip_suffix("[1m]") {
        Some(name) => name.to_owned(),
        None => folded,
    }
}

/// The release name of a Claude id, `None` for an id that is no Claude
/// release or carries anything past its version but a release suffix.
fn claude_release(id: &str) -> Option<String> {
    if id.is_empty() || id.contains(char::is_whitespace) {
        return None;
    }
    let id = id
        .strip_suffix("[1m]")
        .or_else(|| id.strip_suffix("[2m]"))
        .unwrap_or(id);
    let id = id.rsplit('/').next().unwrap_or(id);
    let id = match id.split_once("anthropic.") {
        Some(("", rest)) => rest,
        Some((region, rest)) => match region.strip_suffix('.') {
            Some(region) if REGIONS.contains(&region) => rest,
            Some(region) if region.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') => {
                return None;
            }
            _ => id,
        },
        None => id,
    };
    let (name, suffix) = release_name(id.strip_prefix("claude-")?)?;
    RELEASE_SUFFIX.is_match(suffix).then_some(name)
}

/// `claude-<family>-<major>[-<minor>]` or the older
/// `claude-<major>[-<minor>]-<family>`, from the text after `claude-`, with
/// what follows it. A `-0` minor names the release with no minor, as Claude
/// Code's catalog spells it, except `opus-4` and `sonnet-4`, which it spells
/// `-4-0`.
fn release_name(rest: &str) -> Option<(String, &str)> {
    fn version(text: &str) -> Option<(&str, &str)> {
        let len = text.bytes().take_while(u8::is_ascii_digit).count();
        (1..=2).contains(&len).then(|| text.split_at(len))
    }
    fn minor(text: &str) -> (Option<&str>, &str) {
        match text.strip_prefix('-').and_then(version) {
            Some((minor, tail)) => (Some(minor), tail),
            None => (None, text),
        }
    }
    let letters = |text: &str| text.bytes().take_while(u8::is_ascii_lowercase).count();
    let family_len = letters(rest);
    if family_len > 0 {
        let (family, tail) = rest.split_at(family_len);
        let (major, tail) = version(tail.strip_prefix('-')?)?;
        return Some(match minor(tail) {
            (Some(minor), tail) if minor != "0" => {
                (format!("claude-{family}-{major}-{minor}"), tail)
            }
            (_, tail) if major == "4" && matches!(family, "opus" | "sonnet") => {
                (format!("claude-{family}-4-0"), tail)
            }
            (_, tail) => (format!("claude-{family}-{major}"), tail),
        });
    }
    let (major, tail) = version(rest)?;
    let (minor, tail) = minor(tail);
    let tail = tail.strip_prefix('-')?;
    let family_len = letters(tail);
    if family_len == 0 {
        return None;
    }
    let (family, tail) = tail.split_at(family_len);
    let name = match minor {
        Some(minor) => format!("claude-{major}-{minor}-{family}"),
        None => format!("claude-{major}-{family}"),
    };
    Some((name, tail))
}

/// Whether the settings sync keeps `modelSettings.<key>` per profile: a key
/// an account writes, a key naming the same model as a pinned id once both
/// fold through [`canonical_name`], and every Claude id of a pinned tier
/// alias's tier, since the alias lands on whichever model each session's own
/// `[models]` row names.
pub(crate) fn is_pinned(pinned: &std::collections::BTreeSet<String>, key: &str) -> bool {
    if pinned.contains(key) {
        return true;
    }
    let canonical = canonical_name(key);
    pinned.iter().any(|pin| {
        if TIERS.contains(&pin.as_str()) {
            tier_of_id(&canonical) == Some(pin.as_str())
        } else {
            canonical_name(pin) == canonical
        }
    })
}

/// Cover the keys of the settings that Claude Code would read ahead of the
/// account's own: an existing key folding to the same model as one of the
/// account's takes that model's level (the first account key of that model in
/// [`resolved`]'s order), and any other `claude-<tier>-*` key of a tier the
/// account sets by its bare alias takes the alias's level. An account's own
/// key is never covered.
pub(crate) fn expand_tier_levels(
    settings: &serde_json::Map<String, serde_json::Value>,
    levels: &mut BTreeMap<String, String>,
) {
    let Some(models) = settings
        .get("modelSettings")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    let mut covers = BTreeMap::new();
    for key in models.keys().filter(|key| !levels.contains_key(*key)) {
        let canonical = canonical_name(key);
        let same_model = levels
            .iter()
            .find(|(id, _)| !TIERS.contains(&id.as_str()) && canonical_name(id) == canonical)
            .map(|(_, level)| level);
        if let Some(level) = same_model.or_else(|| levels.get(tier_of(key)?)) {
            covers.insert(key.clone(), level.clone());
        }
    }
    levels.extend(covers);
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
