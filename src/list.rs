//! `clauth list` — a human-readable account table.
//!
//! Renders over the typed entries `daemon::build_profile_entries` produces —
//! the same entries `build_status` serializes into the body `clauth status
//! --json` prints — so every column sourced from those entries cannot drift
//! from `status`. Presentation only: it reads the on-disk usage caches
//! `build_status` reads and never fetches.
//!
//! Three facts do NOT come from the entries, because they carry neither: the
//! `disabled` and `keyless` flags (both read off `config`) and the `canceled`
//! flag (read off the per-profile usage cache). All three surface in the
//! trailing state marker, so this table shows three states `status --json`
//! does not expose.

use anyhow::Result;

use crate::daemon::{ProfileEntry, build_profile_entries};
use crate::format::format_pct;
use crate::out::out;
use crate::profile::{AppConfig, load_config};
use crate::profile_json::Window;

/// `clauth list [--all|--disabled]` — print the account table. `include_disabled`
/// mirrors `build_profile_entries`'s flag: disabled profiles are hidden by
/// default (the active profile is always kept, disabled or not).
pub(crate) fn run(include_disabled: bool) -> Result<()> {
    let config = load_config()?;
    let entries = build_profile_entries(
        &config,
        config.state.refresh_interval_ms,
        None,
        include_disabled,
    );
    out!("{}", render_table(&config, &entries));
    Ok(())
}

/// One rendered table row. Three sources, because the status entry carries only
/// the first: a single `build_profile_entries` profile entry, `config` for the
/// disabled and keyless flags, and the profile's own `usage_cache.json` for the
/// canceled one (via `profile_json::is_canceled_cached`).
struct Row {
    /// `*` for the active profile, a space otherwise.
    marker: char,
    name: String,
    /// Tier for an anthropic account (`Max 5x`), else the provider name for a
    /// third-party one. Typed off the entry's `tier` field, keeping this in
    /// lockstep with `status`. A canceled subscription reads as its
    /// post-cancellation tier (`Free`) here; [`Row::state_suffix`] is what names
    /// the cancellation.
    plan: String,
    /// 5h / 7d window utilization as `NN%` (share consumed), `-` when no cache.
    five_h: String,
    seven_d: String,
    /// The third-party base url, or `-` for the default Anthropic endpoint.
    endpoint: String,
    disabled: bool,
    /// The MCP roster's own `keyless` flag, spelled the same so the two
    /// surfaces cannot drift.
    keyless: bool,
    canceled: bool,
    /// Label for a usage credential that is dead and will not self-heal
    /// (`fetch_status: "AuthExpired"`), or `None` when it is fine. This table
    /// has no freshness column, so without the suffix the stale window
    /// percentages above read as ordinary live numbers.
    ///
    /// Three labels, because the state has three causes and they want
    /// different actions: a stored session lapsed (`login expired`), none was
    /// ever stored (`login needed`), or an api key the provider rejected
    /// (`key rejected`). An api-key account reaches the second the moment it
    /// gets a typed provider whose quota rides a separate credential, and
    /// "expired" would tell that operator to renew something they never had;
    /// a non-Alibaba account reaches only the third, since it has no session
    /// to lapse.
    usage_login: Option<&'static str>,
}

impl Row {
    fn from_entry(config: &AppConfig, entry: &ProfileEntry) -> Row {
        let typed_name = &entry.name;
        Row {
            marker: if entry.active { '*' } else { ' ' },
            name: entry.name.as_str().to_string(),
            plan: entry
                .tier
                .as_deref()
                .unwrap_or(entry.provider.as_str())
                .to_string(),
            five_h: window_pct(&entry.windows, crate::usage::LABEL_5H),
            seven_d: window_pct(&entry.windows, crate::usage::LABEL_7D),
            endpoint: entry.base_url.as_deref().unwrap_or("-").to_string(),
            disabled: config.find(typed_name).is_some_and(|p| p.is_disabled()),
            keyless: config
                .find(typed_name)
                .is_some_and(|p| p.is_third_party() && !crate::claude::has_inference_auth(p)),
            canceled: crate::profile_json::is_canceled_cached(typed_name),
            usage_login: (entry.fetch_status.as_deref() == Some("AuthExpired")).then(|| {
                let p = config.find(typed_name);
                if p.is_some_and(|p| p.console.is_some()) {
                    "login expired"
                } else if p.is_some_and(|p| p.provider != Some(crate::providers::Provider::Alibaba))
                {
                    // No console session can be the cause here: the verdict can
                    // only come from a 401 on the api key.
                    "key rejected"
                } else {
                    "login needed"
                }
            }),
        }
    }

    /// Trailing state marker: `(disabled)`, `(keyless)`, `(canceled)`,
    /// `(login expired)` /
    /// `(login needed)`, or
    /// any combination. All render rather than one winning — an operator usually
    /// disables an account BECAUSE it died, so letting `disabled` mask
    /// `canceled` is the erasure the Fallback tab's stacked pills already exist
    /// to prevent. This table has no status column, so the suffix is the only
    /// place any of these facts can appear.
    fn state_suffix(&self) -> String {
        let states: Vec<&str> = [
            (self.disabled, "disabled"),
            (self.keyless, "keyless"),
            (self.canceled, "canceled"),
        ]
        .into_iter()
        .filter_map(|(on, label)| on.then_some(label))
        .chain(self.usage_login)
        .collect();
        if states.is_empty() {
            return String::new();
        }
        format!(" ({})", states.join(", "))
    }
}

/// The `utilization_pct` of the window labeled `label`, formatted via
/// [`format_pct`] (drops trailing `.0`); `-` when the profile has no cache
/// or no such window.
fn window_pct(windows: &[Window], label: &str) -> String {
    windows
        .iter()
        .find(|w| w.label == label)
        .map(|w| format_pct(w.utilization_pct))
        .unwrap_or_else(|| "-".to_string())
}

/// Minimum column width: the header vs every cell, counted in `char`s so a
/// multibyte profile name still aligns.
fn col_width<'a>(header: &str, cells: impl Iterator<Item = &'a str>) -> usize {
    cells
        .map(|c| c.chars().count())
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(0)
}

fn render_table(config: &AppConfig, entries: &[ProfileEntry]) -> String {
    if entries.is_empty() {
        return "no accounts yet. add one with `clauth login <name>`.\n".to_string();
    }

    let rows: Vec<Row> = entries.iter().map(|e| Row::from_entry(config, e)).collect();

    // Each header is bound once: `col_width` sizes the column off the same
    // string the header row prints, so the two can never disagree. The two
    // window columns say `USED` because the table stands alone in a pipe, where
    // a bare `5H` over `42%` reads as headroom just as easily as consumption.
    let (h_name, h_plan, h_5h, h_7d) = ("PROFILE", "PLAN", "5H USED", "7D USED");
    // Endpoint is the last column, so it is never padded and needs no width.
    let w_name = col_width(h_name, rows.iter().map(|r| r.name.as_str()));
    let w_plan = col_width(h_plan, rows.iter().map(|r| r.plan.as_str()));
    let w_5h = col_width(h_5h, rows.iter().map(|r| r.five_h.as_str()));
    let w_7d = col_width(h_7d, rows.iter().map(|r| r.seven_d.as_str()));

    // Two leading columns: the 1-char active marker and a separating space.
    let mut out = format!(
        "  {:<w_name$}  {:<w_plan$}  {:>w_5h$}  {:>w_7d$}  ENDPOINT\n",
        h_name, h_plan, h_5h, h_7d,
    );
    for r in &rows {
        out.push_str(&format!(
            "{} {:<w_name$}  {:<w_plan$}  {:>w_5h$}  {:>w_7d$}  {}{}\n",
            r.marker,
            r.name,
            r.plan,
            r.five_h,
            r.seven_d,
            r.endpoint,
            r.state_suffix(),
        ));
    }
    out
}

#[cfg(test)]
#[path = "../tests/inline/list.rs"]
mod tests;
