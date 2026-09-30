//! `clauth proxy list [--json]` — every `clauth-<service>-proxy` on `PATH` or
//! registered, joined with its live entry.
//!
//! Reads `discover` for the binaries on `PATH`, the registry for the rows, the
//! daemon's fresh `status.json` `proxies` array (else the record-only entries,
//! both through [`crate::daemon::proxy_slots`]) for the live state, and each
//! service's `manifest` (concurrently, under one aggregate bound) for a
//! version and contract the live slot does not carry. Read-only: it mints
//! nothing, spawns only `manifest`, and touches no proxy.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::PathBuf;

use anyhow::Result;

use crate::daemon::proxies::{ProxySlot, ProxyState, resolve_binary, unsupervised_slot};
use crate::daemon::{daemon_health, proxy_slots};
use crate::out::{out, outln};
use crate::proxy::{Registry, Service, discover, read_manifest};

/// One listing row, derived once and rendered by either surface. `version`,
/// `contract` and `port` are `None` where nothing carries them: the table
/// renders `-`, the JSON emits `null` (the `jobs --json` precedent).
struct Row {
    service: String,
    version: Option<String>,
    contract: Option<String>,
    enabled: bool,
    port: Option<u16>,
    state: String,
}

/// `clauth proxy list [--json]` — the table, or the array with `--json`.
pub(crate) fn run(json: bool) -> Result<()> {
    let rows = rows(std::env::var_os("PATH").as_deref().unwrap_or_default())?;
    if json {
        outln!("{}", rows_json(&rows));
    } else {
        out!("{}", render_table(&rows));
    }
    Ok(())
}

/// One row per service in the union of `discover(path)` and the registry, in
/// service-name order, joined with the daemon's live (or record-only) slot.
/// A registry that does not read is an error, the way `enable`/`disable` fail
/// on the same file.
fn rows(path: &OsStr) -> Result<Vec<Row>> {
    let registry = Registry::load()?;
    let discovered = discover(path);
    let slots = proxy_slots(daemon_health());
    let slot_by_service: BTreeMap<&str, &ProxySlot> = slots
        .iter()
        .map(|slot| (slot.service.as_str(), slot))
        .collect();

    // Which services' manifests must be read: a registered row's binary when
    // its live slot carries no version+contract, and every discovered binary
    // with no row. Resolved through the daemon's own resolver, so a row whose
    // recorded binary is gone reads the same fallback the daemon would run.
    let mut manifest_targets: Vec<(Service, PathBuf)> = Vec::new();
    for (service, row) in registry.iter() {
        let carries_both = slot_by_service
            .get(service.as_str())
            .is_some_and(|slot| slot.version.is_some() && slot.contract.is_some());
        if !carries_both && let Some(binary) = resolve_binary(service, row) {
            manifest_targets.push((service.clone(), binary));
        }
    }
    for found in &discovered {
        if registry.get(&found.service).is_none() {
            manifest_targets.push((found.service.clone(), found.binary.clone()));
        }
    }
    let manifests = read_manifests(manifest_targets);

    let mut services: BTreeSet<Service> = registry
        .iter()
        .map(|(service, _)| service.clone())
        .collect();
    for found in &discovered {
        services.insert(found.service.clone());
    }

    let mut rows = Vec::new();
    for service in services {
        let row = registry.get(&service);
        let slot = slot_by_service.get(service.as_str()).copied();
        let (version, contract) = match slot {
            Some(slot) if slot.version.is_some() && slot.contract.is_some() => {
                (slot.version.clone(), slot.contract.clone())
            }
            _ => manifests
                .get(&service)
                .and_then(|version_contract| version_contract.clone())
                .map(|(version, contract)| (Some(version), Some(contract)))
                .unwrap_or((None, None)),
        };
        rows.push(Row {
            service: service.as_str().to_string(),
            version,
            contract,
            enabled: row.is_some_and(|row| row.enabled),
            port: row.map(|row| row.port),
            state: match slot {
                Some(slot) => state_word(slot.state),
                // A registered row the fresh feed does not carry yet (the
                // daemon trails the registry by one write) falls back to its
                // record-only verdict, never `not_registered`.
                None => match row {
                    Some(_) => state_word(unsupervised_slot(&service).state),
                    None => "not_registered".to_string(),
                },
            },
        });
    }
    Ok(rows)
}

/// `ProxyState`'s serde name (`rename_all = "snake_case"`), the one string the
/// feed publishes. Derived by serializing the enum itself, never a hand
/// mirror, so a per-variant rename cannot split `list` from `status.json`.
fn state_word(state: ProxyState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Read one `manifest` per service concurrently, each self-bounded at
/// [`crate::proxy::MANIFEST_TIMEOUT`], so N wedged binaries cost one bound,
/// never N × 5 s serial. A refused or unreadable manifest reads as no
/// version/contract (the row renders `-` / `null`). A panicking reader
/// propagates through `scope`, like any other panic on the caller's thread.
fn read_manifests(targets: Vec<(Service, PathBuf)>) -> BTreeMap<Service, Option<(String, String)>> {
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for (service, binary) in targets {
            let results = &results;
            scope.spawn(move || {
                let manifest = read_manifest(&service, &binary);
                results
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push((service, manifest));
            });
        }
    });
    results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .into_iter()
        .map(|(service, manifest)| {
            let version_contract = manifest
                .ok()
                .map(|manifest| (manifest.version, manifest.contract));
            (service, version_contract)
        })
        .collect()
}

fn col_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells
        .map(|c| c.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(0)
}

/// The table cell for a value the row may not have: `-`, the `jobs` precedent.
fn dash(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "-".to_string())
}

fn render_table(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "no clauth proxies. install a clauth-<service>-proxy on PATH, then `clauth proxy enable <service>`.\n"
            .to_string();
    }
    let versions: Vec<String> = rows.iter().map(|r| dash(&r.version)).collect();
    let contracts: Vec<String> = rows.iter().map(|r| dash(&r.contract)).collect();
    let ports: Vec<String> = rows
        .iter()
        .map(|r| {
            r.port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string())
        })
        .collect();
    let enabled: Vec<String> = rows.iter().map(|r| r.enabled.to_string()).collect();
    let w_service = col_width("SERVICE", rows.iter().map(|r| r.service.as_str()));
    let w_version = col_width("VERSION", versions.iter().map(String::as_str));
    let w_contract = col_width("CONTRACT", contracts.iter().map(String::as_str));
    let w_enabled = col_width("ENABLED", enabled.iter().map(String::as_str));
    let w_port = col_width("PORT", ports.iter().map(String::as_str));
    let w_state = col_width("STATE", rows.iter().map(|r| r.state.as_str()));
    let mut out = format!(
        "{:<w_service$}  {:<w_version$}  {:<w_contract$}  {:<w_enabled$}  {:>w_port$}  {:<w_state$}\n",
        "SERVICE", "VERSION", "CONTRACT", "ENABLED", "PORT", "STATE",
    );
    for (index, row) in rows.iter().enumerate() {
        out.push_str(&format!(
            "{:<w_service$}  {:<w_version$}  {:<w_contract$}  {:<w_enabled$}  {:>w_port$}  {:<w_state$}\n",
            row.service, versions[index], contracts[index], enabled[index], ports[index], row.state,
        ));
    }
    out
}

fn rows_json(rows: &[Row]) -> String {
    let array: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
    serde_json::to_string_pretty(&array).unwrap_or_else(|_| "[]".to_string())
}

fn row_json(row: &Row) -> serde_json::Value {
    serde_json::json!({
        "service": row.service,
        "version": row.version,
        "contract": row.contract,
        "enabled": row.enabled,
        "port": row.port,
        "state": row.state,
    })
}

#[cfg(test)]
#[path = "../tests/inline/proxy_list.rs"]
mod tests;
