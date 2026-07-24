//! `~/.clauth/tokens.json` feed (TOK-3) — the daemon's machine-wide
//! token-usage snapshot for the ccsbar menu-bar app.
//!
//! # What it publishes
//!
//! A compact per-period token/cost rollup ([`crate::tokens::build_tokens_snapshot`])
//! covering `today` / `week` / `month` / `lifetime`. Unlike `status.json` (which
//! is per-profile clauth account state), this is Claude Code's OWN local usage
//! history **across every account and profile on the machine** — it is derived
//! from `~/.claude/stats-cache.json` + recent transcripts, the exact source the
//! Tokens tab reads, and has no notion of a clauth profile. It is a rebuildable
//! cache, so it is written with [`atomic_write_600_fast`] (non-durable, like
//! `status.json`).
//!
//! # Wiring
//!
//! [`spawn_tokens_feed`] reuses the TUI's two background workers verbatim:
//! [`crate::tokens::spawn`] (stats-cache parse + 90s transcript top-up) and
//! [`crate::pricing::spawn`] (LiteLLM rate feed, disk-cached, 24h cadence). Each
//! worker owns a typed event channel; two forwarder threads funnel only their
//! `Loaded` events into one [`Feed`] channel, and a single consumer thread holds
//! the latest [`TokenStats`] + [`PriceTable`] and rewrites `tokens.json` on every
//! update. Token stats come from `Loaded` only (a `Base` carries no transcript
//! top-up); a price refresh with stats already in hand re-prices in place.
//!
//! Both home-relative dirs are resolved by the caller ([`super::Daemon::boot`])
//! before any thread detaches — the workers never re-resolve `home_dir()`, which
//! would race a test's `HOME_OVERRIDE` (the feed is `cfg(not(test))`-gated for
//! that reason, mirroring the TUI's `app.rs` token/pricing wiring).

use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;

use crate::logline::logline;
use crate::pricing::{self, PriceTable, PricingEvent};
use crate::profile::atomic_write_600_fast;
use crate::tokens::{self, TokenStats, TokensEvent, build_tokens_snapshot, today_date};

/// The published file name, beside `status.json` under `~/.clauth`.
const TOKENS_FILE: &str = "tokens.json";

/// Latest-value updates funneled from the two typed workers into one channel so a
/// single consumer can hold both and rebuild on either.
enum Feed {
    /// A token-stats top-up completed (`TokensEvent::Loaded`).
    Tokens(Box<TokenStats>),
    /// A fresh/cached price table is available (`PricingEvent::Loaded`).
    Prices(Box<PriceTable>),
}

/// Launch the `tokens.json` feed: spin up the token + pricing workers, funnel
/// their `Loaded` events into one channel, and rewrite `<clauth_dir>/tokens.json`
/// atomically whenever the snapshot changes. Both dirs are pre-resolved by the
/// caller; the spawned threads run for the daemon's lifetime.
pub(crate) fn spawn_tokens_feed(clauth_dir: PathBuf, claude_dir: PathBuf) {
    let (tokens_tx, tokens_rx) = channel::<TokensEvent>();
    let (tokens_refresh_tx, tokens_refresh_rx) = channel::<()>();
    let (pricing_tx, pricing_rx) = channel::<PricingEvent>();
    let (pricing_refresh_tx, pricing_refresh_rx) = channel::<()>();

    // Same background workers the TUI runs. `claude_dir` is passed through to the
    // token loader; the pricing loader resolves its own cache path on THIS thread
    // (never in its detached body), same as here.
    tokens::spawn(
        tokens_tx,
        tokens_refresh_rx,
        claude_dir,
        Some(clauth_dir.clone()),
    );
    pricing::spawn(pricing_tx, pricing_refresh_rx);

    let (feed_tx, feed_rx) = channel::<Feed>();

    // Forwarder: token `Loaded` → Feed::Tokens (Base/Progress/Failed dropped —
    // Base has no top-up, so a snapshot is only ever built from a Loaded).
    {
        let feed_tx = feed_tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = tokens_rx.recv() {
                if let TokensEvent::Loaded(stats) = ev
                    && feed_tx.send(Feed::Tokens(stats)).is_err()
                {
                    break; // consumer gone — nothing left to feed
                }
            }
        });
    }
    // Forwarder: pricing `Loaded` → Feed::Prices (Failed dropped — the last table
    // stays in effect).
    {
        let feed_tx = feed_tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = pricing_rx.recv() {
                if let PricingEvent::Loaded(table) = ev
                    && feed_tx.send(Feed::Prices(table)).is_err()
                {
                    break;
                }
            }
        });
    }
    drop(feed_tx); // only the forwarders send; the consumer holds the sole rx

    let out_path = clauth_dir.join(TOKENS_FILE);
    std::thread::spawn(move || {
        // Hold the refresh senders for the process lifetime: dropping them would
        // disconnect each worker's `recv_timeout` and stop its cadence. The daemon
        // never issues a manual refresh, so they only keep the channels open.
        let _tokens_refresh = tokens_refresh_tx;
        let _pricing_refresh = pricing_refresh_tx;

        let mut stats: Option<TokenStats> = None;
        let mut prices: Option<PriceTable> = None;

        while let Ok(feed) = feed_rx.recv() {
            match feed {
                Feed::Tokens(s) => stats = Some(*s),
                Feed::Prices(p) => prices = Some(*p),
            }
            // Publish once token stats exist (prices are optional — an unpriced
            // snapshot still carries token counts). A price update arriving before
            // the first `Loaded` is held and applied on the first stats.
            if let Some(stats) = stats.as_ref() {
                let body = build_tokens_snapshot(stats, prices.as_ref(), &today_date());
                write_snapshot(&out_path, &body);
            }
        }
    });
}

/// Serialize + atomically write the snapshot. Errors are logged, never fatal —
/// `tokens.json` is a rebuildable cache and must never take the daemon down.
fn write_snapshot(path: &Path, body: &serde_json::Value) {
    match serde_json::to_vec_pretty(body) {
        Ok(json) => {
            if let Err(e) = atomic_write_600_fast(path, &json) {
                logline!("clauth daemon: failed to write tokens.json: {e}");
            }
        }
        Err(e) => logline!("clauth daemon: failed to serialize tokens.json: {e}"),
    }
}
