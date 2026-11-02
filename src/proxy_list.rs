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
use crate::format::{col_width, pad_cells};
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
    /// Why the row reads the state it does: the refused row's registry reason,
    /// or the slot's `reason` for whatever state carries one. `None` when the
    /// row has nothing to explain.
    reason: Option<String>,
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

    let mut services: BTreeSet<String> = registry
        .iter()
        .map(|(service, _)| service.as_str().to_string())
        .collect();
    for found in &discovered {
        services.insert(found.service.as_str().to_string());
    }
    services.extend(registry.misconfigured().map(|(name, _)| name.to_string()));

    let mut rows = Vec::new();
    for name in services {
        let misconfigured = registry.misconfigured_reason(&name);
        let service = Service::parse(&name).ok();
        let row = service.as_ref().and_then(|service| registry.get(service));
        let slot = slot_by_service.get(name.as_str()).copied();
        // The record-only slot a registered row falls back to when the fresh
        // feed does not carry it yet: its state and reason render then, never
        // `not_registered`.
        let fallback = if misconfigured.is_none() && row.is_some() && slot.is_none() {
            service.as_ref().map(unsupervised_slot)
        } else {
            None
        };
        let (version, contract) = if misconfigured.is_some() {
            (None, None)
        } else {
            match slot {
                // A slot's version/contract explain its row's state only where
                // the registry still holds that row; a `not_registered` row
                // takes them from the discovered binary's manifest, never the
                // feed's stale word (the registry wins both ways).
                Some(slot)
                    if row.is_some() && slot.version.is_some() && slot.contract.is_some() =>
                {
                    (slot.version.clone(), slot.contract.clone())
                }
                _ => service
                    .as_ref()
                    .and_then(|service| manifests.get(service))
                    .and_then(|version_contract| version_contract.clone())
                    .map(|(version, contract)| (Some(version), Some(contract)))
                    .unwrap_or((None, None)),
            }
        };
        rows.push(Row {
            service: name,
            version,
            contract,
            enabled: row.is_some_and(|row| row.enabled),
            port: row.map(|row| row.port),
            state: if misconfigured.is_some() {
                // The refused row reads `misconfigured`, even where a stale
                // live slot still carries it: the registry wins.
                "misconfigured".to_string()
            } else if service.is_some() {
                match row {
                    Some(_) => match slot {
                        Some(slot) => state_word(slot.state),
                        // A registered row the fresh feed does not carry yet
                        // (the daemon trails the registry by one write) falls
                        // back to its record-only verdict, never
                        // `not_registered`.
                        None => state_word(
                            fallback
                                .as_ref()
                                .map(|slot| slot.state)
                                .unwrap_or(ProxyState::Unobserved),
                        ),
                    },
                    // The registry wins: a service it no longer holds reads
                    // `not_registered`, never the feed's stale word.
                    None => "not_registered".to_string(),
                }
            } else {
                "not_registered".to_string()
            },
            reason: misconfigured
                .map(str::to_string)
                .or_else(|| {
                    // A slot's reason explains its row's state only where the
                    // registry still holds that row; a `not_registered` row
                    // carries no reason (the registry wins both ways).
                    row.is_some()
                        .then_some(slot)
                        .flatten()
                        .and_then(|slot| slot.reason.clone())
                })
                .or_else(|| fallback.and_then(|slot| slot.reason)),
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
        "{}  {}  {}  {}  {}  {}\n",
        pad_cells("SERVICE", w_service, false),
        pad_cells("VERSION", w_version, false),
        pad_cells("CONTRACT", w_contract, false),
        pad_cells("ENABLED", w_enabled, false),
        pad_cells("PORT", w_port, true),
        pad_cells("STATE", w_state, false),
    );
    for (index, row) in rows.iter().enumerate() {
        out.push_str(&format!(
            "{}  {}  {}  {}  {}  {}\n",
            pad_cells(&row.service, w_service, false),
            pad_cells(&versions[index], w_version, false),
            pad_cells(&contracts[index], w_contract, false),
            pad_cells(&enabled[index], w_enabled, false),
            pad_cells(&ports[index], w_port, true),
            pad_cells(&row.state, w_state, false),
        ));
    }
    // After the table, one blank line, then one `<service>: <reason>` line per
    // row that has a reason, in row order; none when no row has one.
    let reason_rows: Vec<&Row> = rows.iter().filter(|row| row.reason.is_some()).collect();
    if !reason_rows.is_empty() {
        out.push('\n');
        for row in reason_rows {
            out.push_str(&format!(
                "{}: {}\n",
                row.service,
                row.reason.as_deref().unwrap_or_default()
            ));
        }
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
        "reason": row.reason,
    })
}

#[cfg(test)]
#[path = "../tests/inline/proxy_list.rs"]
mod tests;
