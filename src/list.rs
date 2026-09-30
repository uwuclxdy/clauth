//! `clauth list` — a human-readable account table.
//!
//! Renders over the typed entries `daemon::build_profile_entries` produces —
//! the same entries `build_status` serializes into the body `clauth status
//! --json` prints — so every column sourced from those entries cannot drift
//! from `status`. The codex roster renders as its own section under that
//! table, over the entries `daemon::build_codex_entries` produces, which
//! `build_status` appends after the claude ones. Presentation only: it reads
//! the on-disk usage caches `build_status` reads and never fetches.
//!
//! Three facts do NOT come from the entries, because they carry neither: the
//! `disabled` and `keyless` flags (both read off `config`) and the `canceled`
//! flag (read off the per-profile usage cache). All three surface in the
//! trailing state marker, so this table shows three states `status --json`
//! does not expose. All three are claude-only: a codex row reads none of them.

use anyhow::Result;

use crate::codex_profiles::CodexState;
use crate::daemon::{ProfileEntry, build_codex_entries, build_profile_entries};
use crate::format::format_pct;
use crate::harness::Harness;
use crate::out::out;
use crate::profile::{AppConfig, load_config};
use crate::profile_json::Window;

/// `clauth list [--all|--disabled]` — print the account table. `include_disabled`
/// mirrors `build_profile_entries`'s flag: disabled profiles are hidden by
/// default (the active profile is always kept, disabled or not). A codex
/// profile has no disabled state, so the flag leaves the codex section alone.
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

/// One rendered table row. Three sources for a claude row, because the status
/// entry carries only the first: a single `build_profile_entries` profile
/// entry, `config` for the disabled and keyless flags, and the profile's own
/// `usage_cache.json` for the canceled one (via
/// `profile_json::is_canceled_cached`). A codex row reads its entry alone.
struct Row {
    /// `*` for the active profile, a space otherwise.
    marker: char,
    name: String,
    /// Tier for an anthropic account (`Max 5x`), else the provider name for a
    /// third-party one; a codex account's ChatGPT plan, else `-`, since its
    /// provider names no plan. Typed off the entry's `tier` field, keeping this
    /// in lockstep with `status`. A canceled subscription reads as its
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
    /// The entry's distrusted-reading flag, rendered as `(stale)`.
    stale: bool,
    /// Labels for dead credentials the user must act on. Two sources that
    /// can both fire on one hybrid (an OAuth pair plus a provider endpoint):
    /// `auth_status: "broken"` (the OAuth credential is dead — re-auth) and
    /// `fetch_status: "AuthExpired"` (the usage credential is dead and will not
    /// self-heal). This table has no freshness column, so without the suffix
    /// the stale window percentages above read as ordinary live numbers.
    ///
    /// Three fetch labels, because that state has three causes and they want
    /// different actions: a stored session lapsed (`login expired`), none was
    /// ever stored (`login needed`), or an api key the provider rejected
    /// (`key rejected`). An api-key account reaches the second the moment it
    /// gets a typed provider whose quota rides a separate credential, and
    /// "expired" would tell that user to renew something they never had;
    /// a non-Alibaba account reaches only the third, since it has no session
    /// to lapse.
    usage_login: [Option<&'static str>; 2],
}

impl Row {
    fn from_entry(config: &AppConfig, entry: &ProfileEntry) -> Row {
        let typed_name = &entry.name;
        // A codex row reads its entry alone. The claude-only reads below would
        // answer off a file the codex name shares: the cache write admits a
        // name either roster holds, so a claude body can land in a codex
        // profile's `usage_cache.json`.
        let claude = entry.harness != Harness::Codex.as_str();
        let profile = if claude {
            config.find(typed_name)
        } else {
            None
        };
        // A third-party account renders its own headroom in these columns —
        // its live cached bars, or the wallet a scalar provider publishes —
        // rather than the store-derived windows the walk judges, and that
        // holds for the accounts whose provider now publishes 5h/7d windows
        // too: the columns stay the provider's figures.
        let (five_h, seven_d) = match profile {
            Some(p) if p.usage_cache_is_third_party() => {
                let (five, seven) = crate::profile_json::third_party_columns(p);
                (
                    five.unwrap_or_else(|| "-".to_string()),
                    seven.unwrap_or_else(|| "-".to_string()),
                )
            }
            _ => (
                window_pct(&entry.windows, crate::usage::LABEL_5H),
                window_pct(&entry.windows, crate::usage::LABEL_7D),
            ),
        };
        Row {
            marker: if entry.active { '*' } else { ' ' },
            name: entry.name.as_str().to_string(),
            plan: entry
                .tier
                .as_deref()
                .unwrap_or(if claude { entry.provider.as_str() } else { "-" })
                .to_string(),
            five_h,
            seven_d,
            endpoint: entry.base_url.as_deref().unwrap_or("-").to_string(),
            disabled: profile.is_some_and(|p| p.is_disabled()),
            keyless: profile
                .is_some_and(|p| p.is_third_party() && !crate::claude::has_inference_auth(p)),
            canceled: claude && crate::profile_json::is_canceled_cached(typed_name),
            stale: entry.stale,
            usage_login: [
                (entry.auth_status.as_str() == "broken").then_some("login expired"),
                (entry.fetch_status.as_deref() == Some("AuthExpired")).then(|| {
                    if profile.is_some_and(|p| p.console.is_some()) {
                        "login expired"
                    } else if profile
                        .is_some_and(|p| p.provider != Some(crate::providers::Provider::Alibaba))
                    {
                        // No console session can be the cause here: the verdict can
                        // only come from a 401 on the api key.
                        "key rejected"
                    } else {
                        "login needed"
                    }
                }),
            ],
        }
    }

    /// Trailing state marker: `(disabled)`, `(keyless)`, `(canceled)`,
    /// `(stale)`, `(login expired)` /
    /// `(login needed)`, or
    /// any combination. All render rather than one winning — a user usually
    /// disables an account BECAUSE it died, so letting `disabled` mask
    /// `canceled` is the erasure the Fallback tab's stacked pills already exist
    /// to prevent. One exception: the two dead-credential sources can render
    /// the SAME label (`login expired` from a broken OAuth pair and from a
    /// lapsed console), and the identical label twice says nothing the once
    /// does, so adjacent duplicates collapse. This table has no status column,
    /// so the suffix is the only place any of these facts can appear.
    fn state_suffix(&self) -> String {
        let mut states: Vec<&str> = [
            (self.disabled, "disabled"),
            (self.keyless, "keyless"),
            (self.canceled, "canceled"),
            (self.stale, "stale"),
        ]
        .into_iter()
        .filter_map(|(on, label)| on.then_some(label))
        .collect();
        for label in self.usage_login {
            if let Some(l) = label
                && !states.contains(&l)
            {
                states.push(l);
            }
        }
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
    let codex_entries = build_codex_entries(
        &CodexState::load().unwrap_or_default(),
        config.state.refresh_interval_ms,
    );
    if entries.is_empty() && codex_entries.is_empty() {
        return "no accounts yet. add one with `clauth login <name>`.\n".to_string();
    }

    let rows: Vec<Row> = entries.iter().map(|e| Row::from_entry(config, e)).collect();
    let codex_rows: Vec<Row> = codex_entries
        .iter()
        .map(|e| Row::from_entry(config, e))
        .collect();

    // Each header is bound once: `col_width` sizes the column off the same
    // string the header row prints, so the two can never disagree. The two
    // window columns say `USED` because the table stands alone in a pipe, where
    // a bare `5H` over `42%` reads as headroom just as easily as consumption.
    let (h_name, h_codex, h_plan, h_5h, h_7d) = ("PROFILE", "CODEX", "PLAN", "5H USED", "7D USED");
    // Every column is sized over both sections, so the codex rows line up under
    // the claude ones. Endpoint is the claude section's last column, so it is
    // never padded and needs no width.
    let all = || rows.iter().chain(&codex_rows);
    let w_name = col_width(h_name, all().map(|r| r.name.as_str()).chain([h_codex]));
    let w_plan = col_width(h_plan, all().map(|r| r.plan.as_str()));
    let w_5h = col_width(h_5h, all().map(|r| r.five_h.as_str()));
    let w_7d = col_width(h_7d, all().map(|r| r.seven_d.as_str()));
    let columns = |name: &str, plan: &str, five_h: &str, seven_d: &str| {
        format!("{name:<w_name$}  {plan:<w_plan$}  {five_h:>w_5h$}  {seven_d:>w_7d$}")
    };

    // Two leading columns: the 1-char active marker and a separating space.
    let mut out = String::new();
    if !rows.is_empty() {
        out.push_str(&format!(
            "  {}  ENDPOINT\n",
            columns(h_name, h_plan, h_5h, h_7d)
        ));
        for r in &rows {
            out.push_str(&format!(
                "{} {}  {}{}\n",
                r.marker,
                columns(&r.name, &r.plan, &r.five_h, &r.seven_d),
                r.endpoint,
                r.state_suffix(),
            ));
        }
    }
    if !codex_rows.is_empty() {
        if !rows.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("  {}\n", columns(h_codex, h_plan, h_5h, h_7d)));
        // No ENDPOINT cell, so the state suffix follows the 7d cell directly.
        for r in &codex_rows {
            out.push_str(&format!(
                "{} {}{}\n",
                r.marker,
                columns(&r.name, &r.plan, &r.five_h, &r.seven_d),
                r.state_suffix(),
            ));
        }
    }
    out
}

#[cfg(test)]
#[path = "../tests/inline/list.rs"]
mod tests;
