//! Inline tests for `crate::providers` — provider URL matching and the
//! disk-cache roundtrip. DeepSeek-specific mapping tests live in
//! `providers_deepseek.rs`.

use super::*;

// ── Provider::from_base_url ───────────────────────────────────────────────────

#[test]
fn deepseek_matches_exact_base_url() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_matches_base_url_with_path() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com/v1"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_rejects_host_extension() {
    // A bare prefix match would claim these and send the profile's API key
    // to the real provider endpoint.
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com.evil.tld"),
        None
    );
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.community"),
        None
    );
}

#[test]
fn a_userinfo_authority_is_not_the_provider_it_is_prefixed_with() {
    // `https://api.deepseek.com:443@evil.tld` has host `evil.tld` — everything
    // before the `@` is userinfo. Claiming it as DeepSeek pointed the typed
    // usage fetch at the real `api.deepseek.com` with this account's key, which
    // is the same leak the host-extension case above exists to stop, reached
    // through the port delimiter instead of the dot.
    for url in [
        "https://api.deepseek.com:443@evil.tld/v1",
        "https://api.deepseek.com:x@evil.tld",
        "https://api.deepseek.com:@evil.tld",
        "https://api.z.ai:8443@evil.tld",
        "https://token-plan.ap-southeast-1.maas.aliyuncs.com:1@evil.tld/apps/anthropic",
    ] {
        assert_eq!(Provider::from_base_url(url), None, "{url}");
    }

    // The positive leg: a real port, an empty port, and a port with a path all
    // still resolve, so the guard rejects userinfo rather than rejecting `:`.
    for url in [
        "https://api.deepseek.com:443",
        "https://api.deepseek.com:443/v1",
        "https://api.deepseek.com:/v1",
        "https://api.deepseek.com:?k=v",
    ] {
        assert_eq!(
            Provider::from_base_url(url),
            Some(Provider::DeepSeek),
            "{url}"
        );
    }
}

#[test]
fn deepseek_rejects_plain_http_and_unrelated_hosts() {
    assert_eq!(Provider::from_base_url("http://api.deepseek.com"), None);
    assert_eq!(Provider::from_base_url("https://api.anthropic.com"), None);
    assert_eq!(Provider::from_base_url(""), None);
}

#[test]
fn deepseek_matches_uppercase_host() {
    // Hosts are case-insensitive (RFC 3986) — a profile pasted with caps still
    // resolves to the provider rather than falling through to "plain API".
    assert_eq!(
        Provider::from_base_url("https://API.DeepSeek.com/v1"),
        Some(Provider::DeepSeek)
    );
}

#[test]
fn deepseek_matches_explicit_port() {
    assert_eq!(
        Provider::from_base_url("https://api.deepseek.com:443/v1"),
        Some(Provider::DeepSeek)
    );
}

// ── Disk cache ────────────────────────────────────────────────────────────────

use crate::testutil::HomeSandbox;

#[test]
fn disk_cache_roundtrips_stats() {
    let _home = HomeSandbox::new();
    let stats = ThirdPartyStats {
        is_available: true,
        rows: vec![StatRow {
            label: "total".to_string(),
            value: "110.00 USD".to_string(),
            kind: StatRowKind::Body,
        }],
        bars: Vec::new(),
        plan: None,
        endpoint: None,
        best_effort: false,
    };
    crate::testutil::register_names(&["tp-cache-test"]);
    crate::profile_cache::write_profile_cache(
        &crate::profile::ProfileName::from("tp-cache-test"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
        &stats,
    );
    let loaded = crate::profile_cache::load_profile_cache::<ThirdPartyStats>(
        &crate::profile::ProfileName::from("tp-cache-test"),
        crate::profile_cache::THIRD_PARTY_CACHE_FILE,
    )
    .expect("cache present");
    assert!(loaded.is_available);
    assert_eq!(loaded.rows.len(), 1);
    assert_eq!(loaded.rows[0].value, "110.00 USD");
}

#[test]
fn disk_cache_missing_reads_as_none() {
    let _home = HomeSandbox::new();
    assert!(
        crate::profile_cache::load_profile_cache::<ThirdPartyStats>(
            &crate::profile::ProfileName::from("tp-cache-absent"),
            crate::profile_cache::THIRD_PARTY_CACHE_FILE
        )
        .is_none()
    );
}

// ── balance-wallet selection ──────────────────────────────────────────────────

/// The wallet parse is deliberately strict. It reads whichever row a provider
/// singles out as its balance, and every one writes something into one — z.ai's
/// counts tokens under the `total` spelling. Anything that is
/// not exactly one finite amount and one currency code describes no wallet, and
/// a loose parse would mint a rank out of it and order the roster on token counts.
#[test]
fn parse_balance_takes_an_amount_and_a_currency_and_nothing_else() {
    assert_eq!(parse_balance("31.45 USD"), Some(("USD".to_string(), 31.45)));
    assert_eq!(
        parse_balance("1117.65 CNY"),
        Some(("CNY".to_string(), 1117.65))
    );
    for junk in [
        "123.4M  (1.2k calls)", // z.ai's token total
        LOW_BALANCE,            // the refusal `unfunded` appends, which reaches this parser
        "31.45",
        "31.45 USD extra",
        "USD 31.45",
        "31.45 U",
        "31.45 TOOLONG",
        "31.45 US1",
        // A non-finite amount must never rank: it outranks (inf) or sinks
        // below (nan) every real wallet in its currency group.
        "nan USD",
        "inf USD",
        "infinity CNY",
        "-inf USD",
        "",
    ] {
        assert_eq!(parse_balance(junk), None, "must not rank on `{junk}`");
    }
    // Exponent and explicit-sign forms parse as finite numbers, so they stay
    // accepted: they order sanely, and refusing them would silently drop the
    // wallet rank of an unknown provider that spelled its total that way.
    assert_eq!(parse_balance("1e3 USD"), Some(("USD".to_string(), 1000.0)));
    assert_eq!(parse_balance("+1.5 USD"), Some(("USD".to_string(), 1.5)));
    // An overdrawn openrouter wallet: the sign parses and the negated-amount
    // sort key puts it after every positive wallet in its currency group.
    assert_eq!(parse_balance("-0.20 USD"), Some(("USD".to_string(), -0.2)));
}

/// The selector the ruling (2026-08-28) is built on, driven from the captured
/// two-wallet cache: the empty USD wallet drops, the funded CNY one stays, and
/// the row's own label and value ride along for the surfaces that render them.
#[test]
fn funded_wallets_drops_empty_wallets_and_keeps_row_order() {
    let stats: ThirdPartyStats =
        serde_json::from_str(crate::testutil::CAPTURED_TWO_WALLET_DS_CACHE)
            .expect("captured cache parses");
    let funded = funded_wallets(&stats.rows);
    assert_eq!(
        funded,
        vec![Wallet {
            label: "api balance".to_string(),
            value: "498.18 CNY".to_string(),
            currency: "CNY".to_string(),
            amount: 498.18,
        }],
        "the 0.00 USD wallet carries no headroom and must not survive the selection",
    );
}

/// [`balance_wallets`] keeps the zero-amount wallets [`funded_wallets`] drops:
/// an all-empty account still renders its figure, so the raw list must stay
/// available to the surface that draws it.
#[test]
fn balance_wallets_keeps_zero_amount_wallets_in_row_order() {
    let stats: ThirdPartyStats =
        serde_json::from_str(crate::testutil::CAPTURED_TWO_WALLET_DS_CACHE)
            .expect("captured cache parses");
    let all = balance_wallets(&stats.rows);
    assert_eq!(
        all.iter()
            .map(|w| (w.value.as_str(), w.amount))
            .collect::<Vec<_>>(),
        vec![("0.00 USD", 0.0), ("498.18 CNY", 498.18)],
        "both wallets parse, in the cache's own row order",
    );
}

// ── api_origin ───────────────────────────────────────────────────────────────

#[test]
fn api_origin_strips_path_to_scheme_host() {
    assert_eq!(
        api_origin("https://api.z.ai/api/anthropic").as_deref(),
        Some("https://api.z.ai")
    );
    assert_eq!(
        api_origin("https://api.deepseek.com/v1").as_deref(),
        Some("https://api.deepseek.com")
    );
}

#[test]
fn api_origin_keeps_port_drops_query_and_fragment() {
    assert_eq!(
        api_origin("https://host.example:8443/path?x=1#frag").as_deref(),
        Some("https://host.example:8443")
    );
}

#[test]
fn api_origin_none_without_scheme_delimiter() {
    assert!(api_origin("api.z.ai/usage").is_none());
}

// ── ThirdPartyTarget::throttle_key ─────────────────────────────────────────────

#[test]
fn throttle_key_known_provider_uses_canonical_origin() {
    // Distinct providers key distinct hosts so they pace independently.
    assert_eq!(
        ThirdPartyTarget::Known {
            provider: Provider::DeepSeek,
            console: None,
        }
        .throttle_key(),
        "https://api.deepseek.com"
    );
    assert_eq!(
        ThirdPartyTarget::Known {
            provider: Provider::Zai,
            console: None,
        }
        .throttle_key(),
        "https://api.z.ai"
    );
    assert_eq!(
        ThirdPartyTarget::Known {
            provider: Provider::OpenRouter,
            console: None,
        }
        .throttle_key(),
        "https://openrouter.ai"
    );
}

#[test]
fn throttle_key_generic_strips_to_origin() {
    // Two api-key profiles on the same host collapse to one pacing key (serialize);
    // a different host yields a different key (parallel).
    assert_eq!(
        ThirdPartyTarget::Generic {
            base_url: "https://proxy.example/v1".to_string(),
        }
        .throttle_key(),
        "https://proxy.example"
    );
}

#[test]
fn throttle_key_generic_falls_back_to_raw_when_schemeless() {
    // No `://` to parse an origin from — the raw base URL is still a stable key.
    assert_eq!(
        ThirdPartyTarget::Generic {
            base_url: "localhost:1234".to_string(),
        }
        .throttle_key(),
        "localhost:1234"
    );
}

// ── Provider::console_url ─────────────────────────────────────────────────────

#[test]
fn console_url_is_the_vendor_page_per_provider() {
    // Exact values: a typo here sends an operator to the wrong product's console.
    assert_eq!(
        Provider::DeepSeek.console_url("https://api.deepseek.com"),
        Some("https://platform.deepseek.com/api_keys")
    );
    assert_eq!(
        Provider::Zai.console_url("https://api.z.ai/api/anthropic"),
        Some("https://z.ai/manage-apikey/apikey-list")
    );
    assert_eq!(
        Provider::OpenRouter.console_url("https://openrouter.ai/api"),
        Some("https://openrouter.ai/settings/keys")
    );
}

#[test]
fn console_url_answers_none_for_a_base_url_the_provider_does_not_own() {
    // The mismatched pair a caller could build by hand. Opening some other
    // account's console would be worse than offering nothing, and the
    // single-page providers are exactly where a wrong page reads as harmless —
    // so every arm re-checks, not just Alibaba's.
    assert_eq!(
        Provider::Alibaba.console_url("https://api.deepseek.com"),
        None
    );
    assert_eq!(
        Provider::OpenRouter.console_url("https://api.deepseek.com"),
        None
    );
    assert_eq!(Provider::DeepSeek.console_url("https://api.z.ai"), None);
    assert_eq!(
        Provider::Zai.console_url("https://token-plan.ap-southeast-1.maas.aliyuncs.com"),
        None
    );
    assert_eq!(Provider::DeepSeek.console_url(""), None);
}

// ── ThirdPartyStats::to_usage_info ────────────────────────────────────────────

fn stats_with_bars(bars: Vec<UsageBar>) -> ThirdPartyStats {
    ThirdPartyStats {
        is_available: true,
        rows: Vec::new(),
        bars,
        plan: None,
        endpoint: None,
        best_effort: false,
    }
}

fn bar(label: &str, pct: f64) -> UsageBar {
    UsageBar {
        label: label.to_string(),
        pct,
        resets_at: Some("2026-09-04T15:00:00+00:00".to_string()),
        used: None,
        total: None,
    }
}

#[test]
fn to_usage_info_maps_the_two_windows_the_chain_judges() {
    let usage = stats_with_bars(vec![bar("5h", 62.0), bar("7d", 31.0)])
        .to_usage_info()
        .expect("both windows present");
    assert_eq!(usage.five_hour.as_ref().map(|w| w.utilization), Some(62.0));
    assert_eq!(usage.seven_day.as_ref().map(|w| w.utilization), Some(31.0));
    assert_eq!(
        usage.five_hour.and_then(|w| w.resets_at).as_deref(),
        Some("2026-09-04T15:00:00+00:00"),
        "the reset instant is what makes the window judgeable as live"
    );
}

#[test]
fn to_usage_info_drops_windows_that_are_not_5h_or_7d() {
    // z.ai's 30d ceiling is account-wide, not per-model, so folding it into
    // `weekly_scoped` would block the member as though one model were capped.
    let usage = stats_with_bars(vec![bar("5h", 10.0), bar("30d", 99.0)])
        .to_usage_info()
        .expect("the 5h window still maps");
    assert!(usage.seven_day.is_none());
    assert!(usage.weekly_scoped.is_empty());
}

#[test]
fn to_usage_info_declines_a_stats_with_no_recognised_window() {
    assert!(stats_with_bars(Vec::new()).to_usage_info().is_none());
    assert!(
        stats_with_bars(vec![bar("30d", 5.0)])
            .to_usage_info()
            .is_none(),
        "a balance-only or monthly-only provider contributes no chain window"
    );
}

#[test]
fn to_usage_info_refuses_best_effort_stats() {
    // The generic scanner's guess at an unknown endpoint is a figure nobody
    // verified; believing it would park an account out of the rotation.
    let mut stats = stats_with_bars(vec![bar("5h", 99.0)]);
    stats.best_effort = true;
    assert!(stats.to_usage_info().is_none());
}

#[test]
fn to_usage_info_clamps_a_bar_into_the_utilization_range() {
    let usage = stats_with_bars(vec![bar("5h", 140.0)])
        .to_usage_info()
        .expect("still a window");
    assert_eq!(usage.five_hour.map(|w| w.utilization), Some(100.0));
}
