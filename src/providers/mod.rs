//! Third-party API provider integration.
//!
//! Recognises providers by base URL and fetches provider-specific statistics
//! for display on the Usage and Setup tabs. Each provider lives in its own
//! submodule; this module owns the shared model, HTTP helper, and disk cache.
//!
//! Adding a provider:
//! 1. Create `src/providers/<name>.rs` with `DISPLAY_NAME`, `matches_base_url`,
//!    and `fetch` (mirror [`deepseek`] for balances, [`zai`] for limit bars +
//!    per-model token rows).
//! 2. Add a variant to [`Provider`] and wire it into `from_base_url`,
//!    `display_name`, `console_url` (the page an operator mints the key on —
//!    cite where the vendor publishes it, since a wrong one sends someone to
//!    another product), [`ThirdPartyTarget::throttle_key`], and
//!    [`Provider::fetch`].
//! 3. Decide what the fetch AUTHENTICATES with before writing it. The api key is
//!    not a given: Alibaba's reads inference only and every quota surface
//!    ignores it, so [`alibaba`] runs on a separate per-profile console session
//!    and its entries are collected even when the api key is absent
//!    ([`crate::usage::third_party_credentialed`] is the shared test, and the
//!    render layer reads the same one). A provider whose usage credential can
//!    die with no refresh path returns [`ThirdPartyError::AuthExpired`] rather
//!    than a generic failure, which is what stops the cadence and tells the
//!    operator to re-authenticate instead of waiting. The shared `get_json`
//!    already maps a 401 to it — a dead api key has no refresh path either — so
//!    a provider only needs to produce the verdict itself when its credential
//!    is session-shaped (Alibaba) and the death arrives in an HTTP 200 body.
//!
//! No render-layer changes needed — [`ThirdPartyStats`] carries provider-agnostic
//! [`UsageBar`]s (percentage windows) and [`StatRow`]s (text), which
//! [`crate::tui::render::usage`] renders uniformly. Unknown api-key providers go
//! through [`generic`]'s best-effort scanner, which sets `best_effort` so the UI
//! invites a bug report.

pub(crate) mod alibaba;
mod deepseek;
mod generic;
mod minimax;
mod openrouter;
mod zai;

pub(crate) use deepseek::BALANCE_ROW_LABEL as DEEPSEEK_BALANCE_ROW_LABEL;

/// Whether a `StatRow` label names the account's spendable balance, in either
/// spelling a cache on disk can carry: the current [`DEEPSEEK_BALANCE_ROW_LABEL`],
/// or the legacy `total` an older clauth wrote and the generic scanner still
/// passes an endpoint's own key through as. Every reader that singles the
/// wallet row out — the overview balance column, the MCP roster's rank and its
/// rendered figure — asks this, so a rename lives in one place.
pub(crate) fn is_balance_row(label: &str) -> bool {
    label == DEEPSEEK_BALANCE_ROW_LABEL || label == "total"
}

/// One wallet parsed off a cached balance row: `"1132.60 CNY"` → currency
/// `CNY`, amount `1132.6`. The row's own `label` and `value` ride along for
/// the surfaces that render the row rather than the figure.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wallet {
    pub(crate) label: String,
    pub(crate) value: String,
    pub(crate) currency: String,
    pub(crate) amount: f64,
}

/// Parse one balance row's value: `"31.45 USD"` → `("USD", 31.45)`: one finite
/// amount plus one 2-5 letter ASCII currency code. The narrowness is the
/// point: a balance row carrying anything else (z.ai's `123.4M  (1.2k calls)`,
/// a second word, `nan`/`inf`) describes no wallet. A loose parse would invent
/// one to rank on.
pub(crate) fn parse_balance(value: &str) -> Option<(String, f64)> {
    let mut parts = value.split_whitespace();
    let amount: f64 = parts.next()?.parse().ok()?;
    if !amount.is_finite() {
        return None;
    }
    let currency = parts.next()?;
    if parts.next().is_some()
        || !(2..=5).contains(&currency.len())
        || !currency.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    Some((currency.to_string(), amount))
}

/// Every balance row parsed into a [`Wallet`], in row order — zero-amount
/// wallets included, so a surface can still render an all-empty account's
/// figure. [`funded_wallets`] is this minus the wallets that carry no headroom.
pub(crate) fn balance_wallets(rows: &[StatRow]) -> Vec<Wallet> {
    rows.iter()
        .filter(|r| is_balance_row(&r.label))
        .filter_map(|r| {
            let (currency, amount) = parse_balance(&r.value)?;
            Some(Wallet {
                label: r.label.clone(),
                value: r.value.clone(),
                currency,
                amount,
            })
        })
        .collect()
}

/// [`balance_wallets`] with the wallets that carry no headroom dropped: a
/// wallet whose amount is not above zero names a pool nothing is left to
/// spend from, and dropping it compares nothing across currencies (owner
/// ruling 2026-08-28). The first element — ROW order, never amount, so two
/// funded wallets of different currencies are never compared — is the wallet
/// the MCP roster ranks the account on and its rendered figure reports; the
/// overview balance column shows the whole list.
pub(crate) fn funded_wallets(rows: &[StatRow]) -> Vec<Wallet> {
    balance_wallets(rows)
        .into_iter()
        .filter(|w| w.amount > 0.0)
        .collect()
}

use serde::{Deserialize, Serialize};

use crate::profile::ConsoleCredential;

// ── Provider ────────────────────────────────────────────────────────────────────

/// Recognised third-party API provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Provider {
    DeepSeek,
    Zai,
    Alibaba,
    OpenRouter,
    MiniMax,
}

impl Provider {
    /// Match a base URL to a known provider. `None` when unrecognised.
    pub(crate) fn from_base_url(url: &str) -> Option<Self> {
        if deepseek::matches_base_url(url) {
            Some(Self::DeepSeek)
        } else if zai::matches_base_url(url) {
            Some(Self::Zai)
        } else if alibaba::matches_base_url(url) {
            Some(Self::Alibaba)
        } else if openrouter::matches_base_url(url) {
            Some(Self::OpenRouter)
        } else if minimax::matches_base_url(url) {
            Some(Self::MiniMax)
        } else {
            None
        }
    }

    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::DeepSeek => deepseek::DISPLAY_NAME,
            Self::Zai => zai::DISPLAY_NAME,
            Self::Alibaba => alibaba::DISPLAY_NAME,
            Self::OpenRouter => openrouter::DISPLAY_NAME,
            Self::MiniMax => minimax::DISPLAY_NAME,
        }
    }

    /// Whether this provider publishes usage windows of its own (percentage
    /// bars under 5h/7d-style labels) rather than a scalar balance. `Zai` and
    /// `Alibaba` do; `DeepSeek` and `OpenRouter` publish a wallet. The MCP
    /// headroom clause denies a 5h/7d limit only where it knows the provider
    /// has none: a windows-publishing provider HAS the limits even when one
    /// cached response carried no bars.
    pub(crate) fn publishes_windows(self) -> bool {
        matches!(self, Self::Zai | Self::Alibaba | Self::MiniMax)
    }

    /// The vendor page where this endpoint's api key is minted, for a surface
    /// that offers to open it. [`alibaba`] answers with four different pages,
    /// since its four endpoints are two products across two consoles; the other
    /// three have one page each.
    ///
    /// `None` means the `base_url` doesn't belong to `self`, which is why every
    /// arm re-checks it rather than only the arm that has to. Returning a page
    /// for a mismatched pair would open some other account's console, and a
    /// single-page provider is exactly where that reads as harmless.
    pub(crate) fn console_url(self, base_url: &str) -> Option<&'static str> {
        match self {
            Self::DeepSeek => deepseek::matches_base_url(base_url).then_some(deepseek::CONSOLE_URL),
            Self::Zai => zai::matches_base_url(base_url).then_some(zai::CONSOLE_URL),
            Self::Alibaba => alibaba::console_url(base_url),
            Self::OpenRouter => {
                openrouter::matches_base_url(base_url).then_some(openrouter::CONSOLE_URL)
            }
            Self::MiniMax => minimax::matches_base_url(base_url).then_some(minimax::CONSOLE_URL),
        }
    }

    /// Fetch this provider's usage. `api_key` authorises every provider except
    /// Alibaba: its quota lives behind the per-profile console session
    /// (`console`), and its api key reads inference only.
    fn fetch(
        self,
        api_key: &str,
        console: Option<&ConsoleCredential>,
    ) -> Result<ThirdPartyStats, ThirdPartyError> {
        match self {
            Self::DeepSeek => deepseek::fetch(api_key),
            Self::Zai => zai::fetch(api_key),
            // The api key is not a quota credential here — the console session is.
            Self::Alibaba => alibaba::fetch(console),
            Self::OpenRouter => openrouter::fetch(api_key),
            Self::MiniMax => minimax::fetch(api_key),
        }
    }
}

/// What a third-party scheduler entry fetches against: a recognised provider
/// (typed fetch) or an unrecognised api-key endpoint (generic discovery + scan).
///
/// `Known` carries the profile's console credential because one provider's usage
/// surface doesn't run on the api key at all: Alibaba's quota lives behind a
/// console session, and its gateway host is picked from that session's
/// region + site rather than being a constant. Every other provider leaves it
/// `None` and is unaffected.
#[derive(Debug, Clone)]
pub(crate) enum ThirdPartyTarget {
    Known {
        provider: Provider,
        console: Option<ConsoleCredential>,
    },
    /// Generic api-key endpoint: usage is discovered + scanned at this base_url's
    /// API origin (same host the key already authorises for completions).
    Generic { base_url: String },
}

impl ThirdPartyTarget {
    /// Origin (`scheme://host`) used as the per-host request-pacing key, so accounts
    /// on the same endpoint serialize while distinct hosts run in parallel. A generic
    /// base URL with no parseable scheme falls back to the raw string — still a
    /// stable per-account key.
    pub(crate) fn throttle_key(&self) -> String {
        match self {
            Self::Known { provider, console } => match provider {
                Provider::DeepSeek => deepseek::ORIGIN.to_string(),
                Provider::Zai => zai::ORIGIN.to_string(),
                // One of four console gateways, chosen by region + site.
                Provider::Alibaba => alibaba::gateway_origin(console.as_ref()).to_string(),
                Provider::OpenRouter => openrouter::ORIGIN.to_string(),
                Provider::MiniMax => minimax::ORIGIN.to_string(),
            },
            Self::Generic { base_url } => api_origin(base_url).unwrap_or_else(|| base_url.clone()),
        }
    }
}

/// `true` when `url` is exactly `base` or `base` followed by a real URL
/// delimiter — path `/`, port `:`, query `?`, or fragment `#`
/// (`https://api.deepseek.com`, `.../v1`, `...:443`), never a host extension
/// (`https://api.deepseek.com.evil.tld`) — a bare `starts_with` would claim
/// those and send the profile's API key to the real provider endpoint.
///
/// A `:` is only a port when what follows it is digits: per RFC 3986 everything
/// before an `@` is USERINFO, so `https://api.deepseek.com:443@evil.tld` has
/// host `evil.tld` and belongs to no provider here. Accepting it labelled that
/// profile DeepSeek and pointed the typed usage fetch at the real
/// `api.deepseek.com`, handing the account's api key to a host its own config
/// never named. An empty port (`https://api.deepseek.com:/v1`) is valid and
/// still the provider, so it matches.
///
/// The scheme + host are compared case-insensitively (hosts are
/// case-insensitive per RFC 3986). `url` is lowercased; `base` is lowercased
/// defensively so a future caller passing mixed-case still matches.
pub(crate) fn url_matches_host(url: &str, base: &str) -> bool {
    let url = url.to_ascii_lowercase();
    let base = base.to_ascii_lowercase();
    match url.strip_prefix(&base) {
        Some("") => true,
        Some(rest) => match rest.strip_prefix(':') {
            Some(after) => {
                let port_end = after.find(['/', '?', '#']).unwrap_or(after.len());
                after[..port_end].bytes().all(|b| b.is_ascii_digit())
            }
            None => rest.starts_with(['/', '?', '#']),
        },
        None => false,
    }
}

/// Derive the API origin (`scheme://host[:port]`) from a base URL, dropping any
/// path/query/fragment. The generic usage engine probes candidate endpoints
/// against this origin only — the api_key never travels to a different host than
/// the one it already authorises. `None` when the `://` scheme delimiter is absent.
pub(crate) fn api_origin(base_url: &str) -> Option<String> {
    let scheme_end = base_url.find("://")?;
    let after = &base_url[scheme_end + 3..];
    let auth_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    Some(format!(
        "{}://{}",
        &base_url[..scheme_end],
        &after[..auth_end]
    ))
}

/// Fetch usage for a third-party target. `hint` is the endpoint path that last
/// yielded data (read from the in-memory store by the caller); only the generic
/// arm uses it, to keep steady state at one request.
pub(crate) fn fetch_third_party_usage(
    target: &ThirdPartyTarget,
    api_key: &str,
    hint: Option<&str>,
) -> Result<ThirdPartyStats, ThirdPartyError> {
    match target {
        ThirdPartyTarget::Known { provider, console } => provider.fetch(api_key, console.as_ref()),
        ThirdPartyTarget::Generic { base_url } => generic::fetch(base_url, api_key, hint),
    }
}

// ── Stats model ─────────────────────────────────────────────────────────────────

/// Provider-agnostic statistics for the Usage tab.
///
/// Each provider's fetch function builds one of these from its API response.
/// The render layer iterates [`rows`](Self::rows) — no per-provider branching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThirdPartyStats {
    /// `false` means the account can't make API calls (e.g. balance exhausted).
    pub(crate) is_available: bool,
    /// Text display rows in source order.
    pub(crate) rows: Vec<StatRow>,
    /// Percentage-based usage windows rendered as bars (e.g. z.ai limits).
    /// Empty for scalar/balance providers that use `rows` instead.
    #[serde(default)]
    pub(crate) bars: Vec<UsageBar>,
    /// Plan/tier label for the header (e.g. "pro"), when the response carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) plan: Option<String>,
    /// Endpoint path that last yielded this data — the generic fetcher reuses it
    /// next tick to skip re-probing. Recognised providers leave this `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) endpoint: Option<String>,
    /// `true` when this came from the best-effort generic scanner (unknown
    /// provider) rather than a typed integration — the render layer shows a
    /// "looks wrong? open an issue" hint. Typed providers leave it `false`.
    #[serde(default)]
    pub(crate) best_effort: bool,
}

/// One percentage-based usage window for bar rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UsageBar {
    pub(crate) label: String,
    /// 0..=100.
    pub(crate) pct: f64,
    /// ISO-8601 reset timestamp when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resets_at: Option<String>,
    /// Absolute amount consumed in the window, when the response carries one
    /// (z.ai `currentValue`). Rendered as the `x` of the bar's trailing `x / y`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) used: Option<f64>,
    /// Absolute window ceiling, when the response carries one — an explicit
    /// total/limit field, or `used + remaining` as a robust fallback (z.ai has
    /// no total field but carries `currentValue` + `remaining`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) total: Option<f64>,
}

impl ThirdPartyStats {
    fn from_rows(rows: Vec<StatRow>) -> Self {
        Self {
            is_available: true,
            rows,
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
        }
    }

    /// The provider's own verdict that the account cannot fund a call, carrying
    /// whatever figures it still reported. The verdict rides as a trailing
    /// `Danger` row instead of replacing the rows: a reader picking a target
    /// needs both how short the account is and that it will refuse, and the
    /// providers that set this flag (DeepSeek's `is_available`, OpenRouter's
    /// overdrawn wallet) both still send the figures.
    fn unfunded(mut rows: Vec<StatRow>) -> Self {
        rows.push(StatRow {
            label: String::new(),
            value: LOW_BALANCE.to_string(),
            kind: StatRowKind::Danger,
        });
        Self {
            is_available: false,
            rows,
            bars: Vec::new(),
            plan: None,
            endpoint: None,
            best_effort: false,
        }
    }
}

impl ThirdPartyStats {
    /// These stats as the [`UsageInfo`] the scheduling layer reads, or `None`
    /// when this provider published no window clauth recognises.
    ///
    /// A provider bar and an OAuth window are the same measurement — a rolling
    /// percentage with a reset instant — so mapping the two labels the chain
    /// actually judges (`5h`, `7d`) lets a third-party member take part in
    /// auto-switch, `clauth list`'s used columns, and the published
    /// `status.json` `windows` array with no per-provider branching downstream.
    ///
    /// Two deliberate exclusions:
    ///
    /// - Any other label (z.ai's `30d`) is dropped rather than folded into
    ///   [`UsageInfo::weekly_scoped`]. That vec means per-MODEL weekly windows
    ///   and carries the `check_scoped` gate's semantics; a monthly account-wide
    ///   ceiling landing there would block the member as though one model were
    ///   capped, which is neither what it measures nor what the gate documents.
    /// - `best_effort` stats — the generic scanner's guess at an unknown
    ///   endpoint's shape — never become windows. A misread field there is a
    ///   figure nobody verified, and the cost of believing it is an account
    ///   parked out of the rotation on a number the vendor never published.
    ///   They keep rendering as bars, which is a claim about the display only.
    pub(crate) fn to_usage_info(&self) -> Option<crate::usage::UsageInfo> {
        if self.best_effort {
            return None;
        }
        let window = |label: &str| {
            self.bars
                .iter()
                .find(|b| b.label == label)
                .map(|b| crate::usage::UsageWindow {
                    utilization: b.pct.clamp(0.0, 100.0),
                    resets_at: b.resets_at.clone(),
                })
        };
        let five_hour = window(crate::usage::LABEL_5H);
        let seven_day = window(crate::usage::LABEL_7D);
        (five_hour.is_some() || seven_day.is_some()).then(|| crate::usage::UsageInfo {
            five_hour,
            seven_day,
            ..Default::default()
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StatRow {
    /// Left-hand label. Empty for single-line messages (e.g. "unavailable").
    pub(crate) label: String,
    /// Right-hand value.
    pub(crate) value: String,
    pub(crate) kind: StatRowKind,
}

/// What every surface says when a provider reports the account cannot fund a
/// call. One spelling, because the roster line, the Usage tab and the MCP
/// headline all render it and a reader meets more than one of them. It is a
/// verdict the PROVIDER reached, never clauth failing to read a figure, which
/// is what the old `balance unavailable` wording claimed.
pub(crate) const LOW_BALANCE: &str = "balance too low";

/// Visual weight of a row in the Usage tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StatRowKind {
    /// Section header (bold, TEXT_DIM + bold).
    Heading,
    /// Normal key:value.
    Body,
    /// Danger-coloured (e.g. [`LOW_BALANCE`]).
    Danger,
    /// Dim / faint text.
    Faint,
}

// ── Error ───────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) enum ThirdPartyError {
    /// Provider returned a non-429, non-401 >=400 status. The caller doesn't
    /// branch on the code — third-party profiles have no chain to rotate — so
    /// it collapses to a cache-fallback like `Network`/`Parse`. A 401 never
    /// reaches this variant: the shared `get_json` maps it to [`AuthExpired`],
    /// since a dead api key can never succeed on retry.
    Status,
    /// HTTP 429. `retry_after` is the server's `retry-after` header in
    /// delta-seconds form (the HTTP-date form is treated as absent), used to
    /// defer this profile's next slot — mirrors the OAuth fetch path.
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },
    Network,
    Parse,
    /// The provider's usage credential is dead or was never captured, and no
    /// refresh path exists — only an operator re-login clears it. Distinct from
    /// `Status` because retrying on the cadence can never succeed: the scheduler
    /// session-suppresses this profile and the UI names the login instead of a
    /// network fault. Two producers: a 401 from the shared `get_json` (a dead
    /// api key), and Alibaba's 48-hour console session (the verdict rides an
    /// HTTP 200 body).
    AuthExpired,
}

// ── HTTP ────────────────────────────────────────────────────────────────────────

fn get_json(url: &str, api_key: &str) -> Result<String, ThirdPartyError> {
    let mut response = crate::usage::http_agent()
        .get(url)
        .header("Authorization", &format!("Bearer {api_key}"))
        .call()
        .map_err(|_| ThirdPartyError::Network)?;
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(crate::usage::parse_retry_after);
        return Err(ThirdPartyError::RateLimited { retry_after });
    }
    if status == 401 {
        // The api key is dead for this host and no refresh path exists, so
        // retrying on the cadence can never succeed — `AuthExpired` is what
        // stops the poll and names a key re-entry instead of a network fault.
        // Shared by every api-key fetch: the typed providers and the generic
        // prober alike.
        return Err(ThirdPartyError::AuthExpired);
    }
    if status >= 400 {
        return Err(ThirdPartyError::Status);
    }
    response
        .body_mut()
        .read_to_string()
        .map_err(|_| ThirdPartyError::Network)
}

// ── Disk cache ──────────────────────────────────────────────────────────────────
//
// Per-profile JSON cache lives in `crate::profile_cache` (shared with the OAuth
// usage layer); this layer only contributes its filename + concrete type.

#[cfg(test)]
#[path = "../../tests/inline/providers.rs"]
mod tests;
