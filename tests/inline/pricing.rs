//! Inline tests for `crate::pricing` — ai-pricelog distill, id resolution,
//! constraint selection, snapshot history, and per-model cost math. No
//! network: tables are built from literals and the trimmed real-index
//! fixture.

use super::*;

use crate::logline::LogLines;
use crate::testutil::HomeSandbox;

/// Trimmed real ai-pricelog v4 index
/// (`tests/fixtures/ai-pricelog-index-trimmed.json`): real rows for the
/// claude / gpt / qwen / glm / deepseek / moonshot / grok first-party
/// representatives, dashscope's six resold rows, deepseek's own
/// `removed_at`-stamped deepseek-r1 row (the delisting pin, first-party so
/// the resold guard cannot dash it instead), one reseller source, and rows
/// marked `"synthetic": true` for shapes no live row exercises: a future
/// `effective_at`, an overlapping window pair, and a `min_tokens` volume
/// tier — every live one of those sits on openrouter, which the resold guard
/// drops (verified against the store, 2026-09-03).
const FIXTURE: &str = include_str!("../fixtures/ai-pricelog-index-trimmed.json");

/// Trimmed real ai-pricelog v4 provider registry
/// (`tests/fixtures/ai-pricelog-providers-trimmed.json`): the eight
/// vendor-carrying providers the other fixtures name plus five vendorless
/// resellers. Verbatim store bytes.
const PROVIDERS_FIXTURE: &str = include_str!("../fixtures/ai-pricelog-providers-trimmed.json");

/// Trimmed real ai-pricelog v4 alias chains
/// (`tests/fixtures/ai-pricelog-aliases-trimmed.json`): the two deepseek api
/// alias chains (`deepseek-chat`, `deepseek-reasoner`) and one null-bound
/// chain (`grok-4.5-latest`). Verbatim store bytes.
const ALIASES_FIXTURE: &str = include_str!("../fixtures/ai-pricelog-aliases-trimmed.json");

// ── helpers ──────────────────────────────────────────────────────────────────

/// The resold guard a literal catalog pair produces, distilled exactly as a
/// fetch does: the provider registry first, then the model registry against
/// it.
fn guard(models: &str, providers: &str) -> FirstParty {
    let vendors = distill_providers(providers).expect("providers distill");
    distill_catalog(models, &vendors)
        .expect("catalog distills")
        .1
}

/// The guard the fixtures' own catalog produces.
fn fixture_guard() -> FirstParty {
    guard(MODELS_FIXTURE, PROVIDERS_FIXTURE)
}

/// The canonical map the fixtures' own catalog produces.
fn fixture_canonical() -> CanonicalMap {
    let vendors = distill_providers(PROVIDERS_FIXTURE).expect("providers distill");
    distill_catalog(MODELS_FIXTURE, &vendors)
        .expect("catalog distills")
        .0
}

/// A guard naming every `(source, model_id)` pair an index JSON carries. The
/// parse-shape tests assert what a row DISTILLS to, so the catalog must not
/// be what decides whether their row survives; the resold-guard tests build a
/// real catalog with [`guard`] instead. An unparseable or shapeless JSON
/// yields an empty guard, which resells everything.
fn all_first_party(index_json: &str) -> FirstParty {
    let root: serde_json::Value =
        serde_json::from_str(index_json).unwrap_or(serde_json::Value::Null);
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    for (source, rows) in root
        .get("sources")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
    {
        for id in rows
            .as_object()
            .into_iter()
            .flatten()
            .map(|(id, _)| id.clone())
        {
            map.entry(source.clone()).or_default().insert(id);
        }
    }
    FirstParty(map)
}

/// One history ndjson's every `(source, model_id)` pair as first-party, the
/// line-shape counterpart of [`all_first_party`].
fn all_first_party_history(ndjson: &str) -> FirstParty {
    let mut map: HashMap<String, HashSet<String>> = HashMap::new();
    for line in ndjson.lines() {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let (Some(source), Some(id)) = (
            row.get("source").and_then(serde_json::Value::as_str),
            row.get("model_id").and_then(serde_json::Value::as_str),
        ) else {
            continue;
        };
        map.entry(source.to_owned())
            .or_default()
            .insert(id.to_owned());
    }
    FirstParty(map)
}

/// One unconstrained price entry at the given input/output rates (cache 0).
fn entry(input: f64, output: f64) -> PriceEntry {
    PriceEntry {
        input,
        output,
        cache_read: 0.0,
        cache_write: 0.0,
        constraint: None,
    }
}

/// An exact-match model at the given input/output rates.
fn eq_model(id: &str, input: f64, output: f64) -> PricedModel {
    PricedModel {
        id: id.to_owned(),
        prices: vec![entry(input, output)],
        effective_at: None,
    }
}

/// A table whose single snapshot (captured 2026-01-01) holds `models`, so any
/// query date resolves to them.
fn table(models: Vec<PricedModel>) -> PriceTable {
    PriceTable {
        models: models.clone(),
        history: vec![RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models,
        }],
        store: Vec::new(),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    }
}

/// The deepseek-v4 window shape: off-peak is half price; peak 01:00–04:00 and
/// 06:00–10:00Z — two whole-hour windows, i.e. two entries.
fn two_window_model() -> PricedModel {
    PricedModel {
        id: "deepseek-v4-pro".to_owned(),
        prices: vec![
            entry(0.2175e-6, 0.435e-6), // off-peak fallback
            PriceEntry {
                input: 0.435e-6,
                output: 0.87e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "01:00".to_owned(),
                    end: "04:00".to_owned(),
                }),
            },
            PriceEntry {
                input: 0.435e-6,
                output: 0.87e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "06:00".to_owned(),
                    end: "10:00".to_owned(),
                }),
            },
        ],
        effective_at: None,
    }
}

fn model(id: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: id.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
    }
}

/// `got` is a resolved rate, `expected` the same rate written as a literal —
/// compared with tolerance, since `mtok / 1e6` and a `x e-N` literal do not
/// always round to the same bits.
fn assert_rate(got: Option<f64>, expected: f64) {
    let got = got.unwrap_or_else(|| panic!("expected {expected}, got None"));
    assert!(
        (got - expected).abs() < 1e-12,
        "got {got}, expected {expected}"
    );
}

// ── distill ──────────────────────────────────────────────────────────────────

#[test]
fn distill_converts_mtok_to_per_token() {
    // Real deepseek-v4-pro row: 0.66 USD/Mtok in → 6.6e-7 per token.
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "deepseek-v4-pro": {"rates": {"input": 0.66, "output": 1.98, "cache_read": 0.022}}
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    assert_eq!(models.len(), 1);
    let rate = &models[0].prices[0];
    assert!((rate.input - 6.6e-7).abs() < 1e-12, "got {}", rate.input);
    assert!((rate.output - 1.98e-6).abs() < 1e-12, "got {}", rate.output);
    assert!((rate.cache_read - 2.2e-8).abs() < 1e-15);
    assert_eq!(rate.cache_write, 0.0); // missing axis defaults to 0
}

#[test]
fn distill_skips_rows_with_malformed_price_fields() {
    // The index carries no tiered ladders (verified against the live index);
    // a declared rate axis of any non-numeric shape fails the whole row,
    // which the caller skips — the sibling row survives.
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "bad-price-shape": {"rates": {"input": "garbage", "output": 0.42}},
        "deepseek-v3.2": {"rates": {"input": 0.28, "output": 0.42}}
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["deepseek-v3.2"]);
}

#[test]
fn distill_ignores_the_v4_keys_it_models_nothing_for() {
    // A real v4 row carries a great deal clauth prices nothing off:
    // `schema` / `source` / `model_id` / `observed_at` / `first_seen`
    // bookkeeping, `provenance`, `fees`, `limits` (the flat `max_tokens`
    // family's replacement), `currency`, and six rate axes with no token
    // bucket to charge to. None is declared, so the row parses and prices its
    // four axes; DECLARING one of the wrong shape is what fails a row.
    //
    // `currency` appears here in the only shape the store writes it — a
    // NON-USD source quote beside the `provenance.fx_rate` that converted it
    // (scaleway's real EUR shape). The rates are already USD, so ignoring the
    // key prices the row correctly; reading it as the rates' unit would not.
    let json = r#"{"version": 4, "sources": {"deepseek": {"deepseek-v4-pro": {
        "schema": 4, "source": "deepseek", "model_id": "deepseek-v4-pro",
        "observed_at": "2026-08-30", "first_seen": "2026-04-24", "currency": "EUR",
        "rates": {"input": 0.66, "output": 1.98, "cache_read": 0.022,
                  "cache_write_1h": 9.9, "image": 9.9, "audio": 9.9,
                  "internal_reasoning": 9.9, "input_audio_cache": 9.9, "image_output": 9.9},
        "fees": {"web_search": 0.005},
        "limits": {"context": 1048576, "output": 393216},
        "provenance": {"url": "https://api-docs.deepseek.com/quick_start/pricing/",
                       "fx_rate": 1.1643, "fx_rate_date": "2026-08-28"}
    }}}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    assert_eq!(models.len(), 1);
    let rate = &models[0].prices[0];
    assert!((rate.input - 6.6e-7).abs() < 1e-12);
    assert!((rate.output - 1.98e-6).abs() < 1e-12);
    assert!((rate.cache_read - 2.2e-8).abs() < 1e-15);
    assert_eq!(rate.cache_write, 0.0, "the 1h axis never fills the 5m one");
}

// ── the resold guard ─────────────────────────────────────────────────────────

/// A literal provider registry: three vendor-carrying makers beside two
/// vendorless resellers.
const GUARD_PROVIDERS: &str = r#"{"version": 4, "providers": {
    "deepseek": {"name": "DeepSeek", "vendor": "deepseek", "kind": "first_party"},
    "dashscope": {"name": "Alibaba Model Studio", "vendor": "alibaba", "kind": "first_party"},
    "anthropic": {"name": "Anthropic", "vendor": "anthropic", "kind": "first_party"},
    "openrouter": {"name": "OpenRouter", "kind": "reseller"},
    "cerebras": {"name": "Cerebras", "kind": "reseller"}
}}"#;

#[test]
fn distill_keeps_first_party_drops_resellers() {
    // A row is resold when the model's maker differs from the provider's
    // vendor, and a provider publishing NO vendor resells everything — so
    // both reseller copies of the id deepseek's own row prices drop, however
    // they spell it.
    let catalog = r#"{"version": 4, "models": {"deepseek-v3.2": {
        "vendor": "deepseek", "curated": true,
        "sources": {"deepseek": ["deepseek-v3.2"],
                    "openrouter": ["deepseek/deepseek-v3.2"],
                    "cerebras": ["deepseek-v3.2"]}
    }}}"#;
    let json = r#"{"version": 4, "sources": {
        "deepseek": {"deepseek-v3.2": {"rates": {"input": 0.28, "output": 0.42}}},
        "openrouter": {"deepseek/deepseek-v3.2": {"rates": {"input": 3, "output": 15}}},
        "cerebras": {"deepseek-v3.2": {"rates": {"input": 5, "output": 25}}}
    }}"#;
    let models = distill(json, &guard(catalog, GUARD_PROVIDERS)).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["deepseek-v3.2"]);
    assert!(
        (models[0].prices[0].input - 2.8e-7).abs() < 1e-12,
        "deepseek's own rate"
    );
}

#[test]
fn distill_splits_a_mixed_provider_row_by_row() {
    // dashscope MAKES qwen (vendor alibaba) and RESELLS deepseek ids. One
    // vendor comparison per row splits them; a provider allowlist could only
    // keep the whole source or drop it.
    let catalog = r#"{"version": 4, "models": {
        "qwen3.8-max": {"vendor": "alibaba", "curated": true,
                        "sources": {"dashscope": ["qwen3.8-max"]}},
        "deepseek-v4-pro": {"vendor": "deepseek", "curated": true,
                            "sources": {"deepseek": ["deepseek-v4-pro"],
                                        "dashscope": ["deepseek-v4-pro"]}}
    }}"#;
    let json = r#"{"version": 4, "sources": {"dashscope": {
        "qwen3.8-max": {"rates": {"input": 2.0, "output": 6.0}},
        "deepseek-v4-pro": {"rates": {"input": 2.4, "output": 4.8}}
    }}}"#;
    let models = distill(json, &guard(catalog, GUARD_PROVIDERS)).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["qwen3.8-max"], "its own vendor's row survives alone");
}

#[test]
fn distill_drops_a_resold_row_whatever_its_id_spells() {
    // The id-prefix guess this replaced could only recognize a resold
    // `claude*` row. The vendor comparison needs no spelling: anthropic's own
    // claude row survives, dashscope's copy drops, and so does dashscope's
    // resale of a NON-claude id — the case no prefix guess reaches.
    let catalog = r#"{"version": 4, "models": {
        "claude-opus-5": {"vendor": "anthropic", "curated": true,
                          "sources": {"anthropic": ["claude-opus-5"],
                                      "dashscope": ["Claude-Opus-5"]}},
        "deepseek-v4-pro": {"vendor": "deepseek", "curated": true,
                            "sources": {"dashscope": ["deepseek-v4-pro"]}}
    }}"#;
    let json = r#"{"version": 4, "sources": {
        "anthropic": {"claude-opus-5": {"rates": {"input": 5, "output": 25}}},
        "dashscope": {
            "Claude-Opus-5": {"rates": {"input": 3, "output": 15}},
            "deepseek-v4-pro": {"rates": {"input": 2.4, "output": 4.8}}
        }
    }}"#;
    let models = distill(json, &guard(catalog, GUARD_PROVIDERS)).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["claude-opus-5"]);
    assert!(
        (models[0].prices[0].input - 5e-6).abs() < 1e-12,
        "anthropic's own rate"
    );
}

#[test]
fn a_model_with_no_named_maker_is_nobody_s_first_party_row() {
    // The clause a bare inequality loses: two absent vendors compare EQUAL,
    // which would hand every maker-less model to whichever provider serves
    // it. The live catalog gives 17 of its 724 entries no vendor and all 17
    // sit on vendorless resellers (measured 2026-09-03), so the row must drop
    // on the vendorless provider AND on the vendor-carrying one.
    let catalog = r#"{"version": 4, "models": {"mystery-9b": {
        "vendor": null, "curated": false,
        "sources": {"openrouter": ["mystery-9b"], "deepseek": ["mystery-9b"]}
    }}}"#;
    let json = r#"{"version": 4, "sources": {
        "openrouter": {"mystery-9b": {"rates": {"input": 1, "output": 2}}},
        "deepseek": {"mystery-9b": {"rates": {"input": 3, "output": 4}}}
    }}"#;
    assert!(
        distill(json, &guard(catalog, GUARD_PROVIDERS)).is_err(),
        "a maker-less model is first-party nowhere, so nothing survives"
    );
}

#[test]
fn a_pair_the_catalog_does_not_name_is_resold() {
    // The catalog is the only place a maker is named, so a row it says
    // nothing about has nothing to compare and drops. The live feeds carry no
    // such pair (0 of 1106 index rows and 0 of 1106 history pairs, measured
    // 2026-09-03); this is what happens when one appears.
    let catalog = r#"{"version": 4, "models": {"deepseek-v3.2": {
        "vendor": "deepseek", "curated": true, "sources": {"deepseek": ["deepseek-v3.2"]}
    }}}"#;
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "deepseek-v3.2": {"rates": {"input": 0.28, "output": 0.42}},
        "deepseek-v9-unlisted": {"rates": {"input": 9, "output": 9}}
    }}}"#;
    let models = distill(json, &guard(catalog, GUARD_PROVIDERS)).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["deepseek-v3.2"]);
}

#[test]
fn catalog_reads_a_sources_value_as_a_list() {
    // Every `sources` value in the v4 catalog is a LIST of that source's
    // spellings — all 1106 of them, zero strings (measured 2026-09-03).
    // Reading one as a string names no pair at all, so the canonical map
    // distills empty and the feed fails; a bare string is still accepted as a
    // one-element list.
    let providers = r#"{"version": 4, "providers": {
        "groq": {"name": "Groq", "kind": "reseller"},
        "deepseek": {"name": "DeepSeek", "vendor": "deepseek", "kind": "first_party"}
    }}"#;
    let catalog = r#"{"version": 4, "models": {"deepseek-v3.2": {
        "vendor": "deepseek", "curated": true,
        "sources": {"deepseek": ["deepseek-v3.2", "deepseek-v3.2-exp"],
                    "groq": "deepseek-v3.2"}
    }}}"#;
    let vendors = distill_providers(providers).expect("providers distill");
    let (canonical, first_party) = distill_catalog(catalog, &vendors).expect("catalog distills");
    // Both list entries land, and so does the bare-string one.
    let deepseek = canonical.get("deepseek").expect("deepseek pairs");
    assert_eq!(
        deepseek.get("deepseek-v3.2"),
        Some(&"deepseek-v3.2".to_owned())
    );
    assert_eq!(
        deepseek.get("deepseek-v3.2-exp"),
        Some(&"deepseek-v3.2".to_owned())
    );
    assert_eq!(
        canonical
            .get("groq")
            .and_then(|ids| ids.get("deepseek-v3.2")),
        Some(&"deepseek-v3.2".to_owned())
    );
    // The guard follows the same read: deepseek makes both spellings, groq
    // publishes no vendor and resells its one.
    assert!(!first_party.resells("deepseek", "deepseek-v3.2"));
    assert!(!first_party.resells("deepseek", "deepseek-v3.2-exp"));
    assert!(first_party.resells("groq", "deepseek-v3.2"));
    // A value of neither shape names nothing, and a catalog of only those
    // fails rather than reselling every row silently.
    let broken = r#"{"version": 4, "models": {"m": {
        "vendor": "deepseek", "sources": {"deepseek": 42}
    }}}"#;
    assert!(distill_catalog(broken, &vendors).is_err());
}

#[test]
fn provider_registry_keeps_only_named_vendors() {
    let vendors = distill_providers(PROVIDERS_FIXTURE).expect("providers distill");
    assert_eq!(
        vendors.get("dashscope").map(String::as_str),
        Some("alibaba")
    );
    assert_eq!(vendors.get("xai").map(String::as_str), Some("xai"));
    // A reseller publishes no vendor and is absent, never mapped to a null.
    for reseller in ["openrouter", "together", "avian", "cloudflare", "groq"] {
        assert!(!vendors.contains_key(reseller), "{reseller}");
    }
    // Tolerance: a missing object and a registry naming no vendor at all both
    // fail rather than reselling every row while looking healthy.
    assert!(distill_providers("{}").is_err());
    assert!(distill_providers("not json").is_err());
    assert!(distill_providers(r#"{"providers": {}}"#).is_err());
    assert!(distill_providers(r#"{"providers": {"groq": {"kind": "reseller"}}}"#).is_err());
    assert!(distill_providers(r#"{"providers": {"groq": {"vendor": 7}}}"#).is_err());
}

// ── the feed URLs and the all-or-nothing rule ────────────────────────────────

#[test]
fn the_five_feed_urls_are_the_published_dist_tree() {
    // A branch or path typo is invisible at compile time and at runtime reads
    // as a plain fetch failure, which `run_fetch` answers by serving the cache
    // — so a warm user would keep stale prices forever with no signal. Each
    // literal is spelled out here so the typo reds instead. The five paths are
    // what ai-pricelog's own publisher writes: `index.json` and
    // `history.ndjson` at the tree root, and every catalog file copied under
    // `catalog/`.
    assert_eq!(
        INDEX_URL,
        "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/index.json"
    );
    assert_eq!(
        HISTORY_URL,
        "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/history.ndjson"
    );
    assert_eq!(
        CATALOG_MODELS_URL,
        "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/catalog/models.json"
    );
    assert_eq!(
        CATALOG_PROVIDERS_URL,
        "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/catalog/providers.json"
    );
    assert_eq!(
        CATALOG_ALIASES_URL,
        "https://raw.githubusercontent.com/uwuclxdy/ai-pricelog/dist/catalog/aliases.json"
    );
    // All five are distinct files under one tree: a copy-paste that points two
    // constants at one path would distill the wrong feed into the wrong half.
    let urls = [
        INDEX_URL,
        HISTORY_URL,
        CATALOG_MODELS_URL,
        CATALOG_PROVIDERS_URL,
        CATALOG_ALIASES_URL,
    ];
    let unique: HashSet<&str> = urls.iter().copied().collect();
    assert_eq!(unique.len(), urls.len());
}

/// The five feed files, served from the fixtures by URL.
fn feed_fixture(url: &str) -> anyhow::Result<String> {
    let body = match url {
        INDEX_URL => FIXTURE,
        HISTORY_URL => HISTORY_FIXTURE,
        CATALOG_MODELS_URL => MODELS_FIXTURE,
        CATALOG_PROVIDERS_URL => PROVIDERS_FIXTURE,
        CATALOG_ALIASES_URL => ALIASES_FIXTURE,
        other => anyhow::bail!("unexpected url {other}"),
    };
    Ok(body.to_owned())
}

#[test]
fn every_feed_file_failing_fails_the_whole_attempt() {
    // The all-or-nothing rule: a cached table must never mix a fresh half with
    // a stale one, so ONE file failing has to fail the composition. The
    // control comes first — with every file served the attempt succeeds, so a
    // later failure is the injected one and not a broken harness.
    let whole = fetch_table_with(feed_fixture).expect("the fixtures compose a feed");
    assert!(!whole.models.is_empty());
    assert!(!whole.store.is_empty());
    assert!(!whole.aliases.is_empty());
    assert!(!whole.canonical.is_empty());

    // Then one file at a time. A `Failed` here reaches the user as `rates
    // unavailable` on a cold start; silently distilling the other four would
    // reach them as a table filtered by half a catalog.
    for down in [
        INDEX_URL,
        HISTORY_URL,
        CATALOG_MODELS_URL,
        CATALOG_PROVIDERS_URL,
        CATALOG_ALIASES_URL,
    ] {
        let fetch = |url: &str| -> anyhow::Result<String> {
            if url == down {
                anyhow::bail!("404 {url}");
            }
            feed_fixture(url)
        };
        assert!(
            fetch_table_with(fetch).is_err(),
            "{down} failing must fail the attempt"
        );
    }
}

// ── override entries ─────────────────────────────────────────────────────────

#[test]
fn distill_parses_overrides_and_effective_at() {
    // deepseek-v4-pro's real shape: base rates, a row-level effective date,
    // and two weekday override entries with HHMM windows.
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "deepseek-v4-pro": {
            "rates": {"input": 0.66, "output": 1.98, "cache_read": 0.022},
            "effective_at": "2026-08-23",
            "overrides": [
                {"when": {"days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
                          "window": [100, 400]},
                 "rates": {"input": 1.32, "output": 3.96, "cache_read": 0.044}},
                {"when": {"days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
                          "window": [600, 1000]},
                 "rates": {"input": 1.32, "output": 3.96, "cache_read": 0.044}}
            ]
        }
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    assert_eq!(models.len(), 1);
    let m = &models[0];
    assert_eq!(m.effective_at.as_deref(), Some("2026-08-23"));
    assert_eq!(m.prices.len(), 3);
    assert_eq!(m.prices[0].constraint, None);
    assert!((m.prices[0].input - 6.6e-7).abs() < 1e-12);
    let weekdays: Vec<&str> = vec!["monday", "tuesday", "wednesday", "thursday", "friday"];
    assert_eq!(
        m.prices[1].constraint,
        Some(Constraint::Days {
            days: weekdays.iter().map(|d| (*d).to_owned()).collect(),
            start: Some("01:00".to_owned()),
            end: Some("04:00".to_owned()),
        })
    );
    assert_eq!(
        m.prices[2].constraint,
        Some(Constraint::Days {
            days: weekdays.iter().map(|d| (*d).to_owned()).collect(),
            start: Some("06:00".to_owned()),
            end: Some("10:00".to_owned()),
        })
    );
    assert!((m.prices[2].input - 1.32e-6).abs() < 1e-12);
    assert!((m.prices[2].cache_read - 4.4e-8).abs() < 1e-15);
}

#[test]
fn distill_override_inherits_missing_axes_from_base() {
    // An override carries only the axes it changes; the ones it leaves absent
    // price at the row's base. Every axis has to DISCRIMINATE, so the base
    // names a distinct non-zero value for all four and the override supplies
    // only `output` and `cache_read`: dropping the `.or(base.…)` on any of the
    // other two then yields 0.0 where a real value is asserted. An override
    // that supplied `input` would make `input`'s inheritance unobservable, and
    // an axis absent from BOTH sides would assert exactly what a broken
    // inherit also returns.
    let json = r#"{"version": 4, "sources": {"zai": {
        "m": {
            "rates": {"input": 0.1, "output": 0.2, "cache_read": 0.01, "cache_write": 1.25},
            "overrides": [
                {"when": {"window": [100, 400]}, "rates": {"output": 0.5, "cache_read": 0.03}}
            ]
        }
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    let entries = &models[0].prices;
    assert_eq!(entries.len(), 2);
    // Supplied by the override.
    assert!((entries[1].output - 5e-7).abs() < 1e-12);
    assert!((entries[1].cache_read - 3e-8).abs() < 1e-15);
    // Absent from the override, so inherited — and non-zero, so a fall to 0.0
    // cannot pass.
    assert!(
        (entries[1].input - 1e-7).abs() < 1e-12,
        "input inherits base, got {}",
        entries[1].input
    );
    assert!(
        (entries[1].cache_write - 1.25e-6).abs() < 1e-12,
        "cache_write inherits base, got {}",
        entries[1].cache_write
    );
    // The base entry itself inherits nothing: an axis absent everywhere is 0.
    let bare = r#"{"version": 4, "sources": {"zai": {
        "m": {"rates": {"input": 0.1, "output": 0.2}}
    }}}"#;
    let bare = distill(bare, &all_first_party(bare)).expect("distill ok");
    assert_eq!(bare[0].prices[0].cache_read, 0.0);
    assert_eq!(bare[0].prices[0].cache_write, 0.0);
}

#[test]
fn distill_skips_quota_only_entries() {
    // `quota_multiplier` is a consumption weight, never a rate: an entry with
    // no `rates` of its own is skipped at distill (the zai glm rows carry
    // these), while a quota key ON a rated entry is ignored and the rates
    // still contribute.
    let json = r#"{"version": 4, "sources": {"zai": {
        "glm-5.3-flash": {
            "rates": {"input": 0.075, "output": 0.25, "cache_read": 0.015},
            "overrides": [
                {"quota_multiplier": 0.4},
                {"when": {"days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
                          "window": [600, 1000]},
                 "quota_multiplier": 1.2}
            ]
        },
        "quota-plus-rates": {
            "rates": {"input": 0.1, "output": 0.2},
            "overrides": [
                {"when": {"window": [100, 400]}, "quota_multiplier": 1.5,
                 "rates": {"input": 0.5}}
            ]
        }
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].prices.len(), 1, "both quota entries skipped");
    assert!((models[0].prices[0].input - 7.5e-8).abs() < 1e-15);
    let rated = &models[1].prices;
    assert_eq!(rated.len(), 2, "the rated entry survives");
    assert!((rated[1].input - 5e-7).abs() < 1e-12);
    assert_eq!(
        rated[1].constraint,
        Some(Constraint::TimeWindow {
            start: "01:00".to_owned(),
            end: "04:00".to_owned(),
        })
    );
}

#[test]
fn distill_skips_volume_tier_overrides() {
    // v3 parked token-volume tiers in a `volume_rates` key clauth never
    // declared, so they could not leak; v4 sits them in the same `overrides`
    // list as the time windows, keyed by `when.min_tokens`. clauth prices per
    // HOUR and has no volume dimension, so the entry is dropped like a
    // quota-only one — kept, it would be unconstrained and would price every
    // request at the tier's rate, 9× the base here.
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "tiered": {
            "rates": {"input": 0.1, "output": 0.2},
            "overrides": [
                {"when": {"min_tokens": 200000}, "rates": {"input": 0.9, "output": 1.8}}
            ]
        },
        "tiered-inside-a-window": {
            "rates": {"input": 0.1, "output": 0.2},
            "overrides": [
                {"when": {"min_tokens": 200000, "window": [100, 400]},
                 "rates": {"input": 0.9}}
            ]
        }
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    assert_eq!(models.len(), 2);
    for m in &models {
        assert_eq!(m.prices.len(), 1, "{}: the tier contributes no entry", m.id);
        assert_eq!(m.prices[0].constraint, None);
    }
    // And through the resolver: every hour prices the base, none the tier.
    let t = table(models);
    for id in ["tiered", "tiered-inside-a-window"] {
        for hour in [0, 2, 3, 12, 23] {
            assert_rate(t.rate_at(id, "2026-08-19", hour).map(|r| r.input), 1e-7);
        }
    }
}

#[test]
fn distill_reads_an_override_window_without_re_deriving_its_timezone() {
    // ai-pricelog converts every schedule to UTC calendar days at build time,
    // so `when.timezone` is provenance for a human. Two rows with the same
    // window and day set must price identically whatever zone they name —
    // shifting by the zone would move an already-shifted schedule twice
    // (Asia/Shanghai is UTC+8: hour 2 would fall out of the window and hour
    // 18 into it).
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "zoned": {
            "rates": {"input": 0.1, "output": 0.2},
            "overrides": [{"when": {"days": ["wednesday"], "window": [100, 400],
                                    "timezone": "Asia/Shanghai"},
                           "rates": {"input": 0.5}}]
        },
        "bare": {
            "rates": {"input": 0.1, "output": 0.2},
            "overrides": [{"when": {"days": ["wednesday"], "window": [100, 400]},
                           "rates": {"input": 0.5}}]
        }
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("distill ok");
    assert_eq!(
        models[0].prices[1].constraint,
        models[1].prices[1].constraint
    );
    let t = table(models);
    // 2026-08-19 is a Wednesday.
    let input = |id: &str, hour: u8| t.rate_at(id, "2026-08-19", hour).map(|r| r.input);
    for id in ["zoned", "bare"] {
        assert_rate(input(id, 2), 5e-7); // inside the UTC window
        assert_rate(input(id, 18), 1e-7); // where a +8 shift would put it
    }
}

#[test]
fn distill_skips_malformed_entries_not_the_row() {
    // An override whose window violates the generator's bounds (hours > 24)
    // skips only itself; the base entry and the good sibling keep the row
    // priced.
    let json = r#"{"version": 4, "sources": {"deepseek": {
        "probe": {
            "rates": {"input": 0.1, "output": 0.2},
            "overrides": [
                {"when": {"window": [9000, 9500]}, "rates": {"input": 0.3}},
                {"when": {"window": [100, 400]}, "rates": {"input": 0.4}}
            ]
        }
    }}}"#;
    let models = distill(json, &all_first_party(json)).expect("good entries survive");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "probe");
    assert_eq!(models[0].prices.len(), 2);
    assert_eq!(models[0].prices[0].constraint, None);
    assert_eq!(
        models[0].prices[1].constraint,
        Some(Constraint::TimeWindow {
            start: "01:00".to_owned(),
            end: "04:00".to_owned(),
        })
    );
    assert!((models[0].prices[1].input - 4e-7).abs() < 1e-12);
}

#[test]
fn distill_fails_when_no_models_survive() {
    // Only resold rows → zero models → the fetch fails rather than shipping
    // an empty table.
    let json = r#"{"version": 4, "sources": {
        "openrouter": {"z-ai/glm-4.7": {"rates": {"input": 0.5, "output": 1}}}
    }}"#;
    assert!(distill(json, &FirstParty::default()).is_err());
    // The genai-prices shape (an array root) is not this feed's shape.
    let arr = r#"[{"id": "deepseek", "models": []}]"#;
    assert!(distill(arr, &all_first_party(arr)).is_err());
    assert!(distill("{}", &all_first_party("{}")).is_err());
    assert!(distill("not json", &all_first_party("not json")).is_err());
}

#[test]
fn distill_skips_unparseable_rows_and_sources() {
    let json = r#"{"version": 4, "sources": {
        "deepseek": {
            "good": {"rates": {"input": 0.28, "output": 0.42}},
            "bad-price-shape": {"rates": {"input": "garbage"}},
            "no-token-price": {"fees": {"web_search": 10}}
        },
        "not-an-object-source": 42
    }}"#;
    let models = distill(json, &all_first_party(json)).expect("one good model survives");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["good"]);
}

#[test]
fn unknown_index_version_warns_and_still_parses() {
    let lines = LogLines::new();
    let _guard = lines.capture_here();
    let json = r#"{"version": 99, "sources": {
        "openai": {"gpt-4.1": {"rates": {"input": 2.0, "output": 8.0}}}
    }}"#;
    let models = distill(json, &all_first_party(json)).expect("parses best-effort");
    assert_eq!(models.len(), 1);
    let missing = r#"{"sources": {
        "openai": {"gpt-4.1": {"rates": {"input": 2.0, "output": 8.0}}}
    }}"#;
    assert_eq!(
        distill(missing, &all_first_party(missing))
            .expect("parses best-effort")
            .len(),
        1
    );
    // The version this build parses is the one that must NOT warn.
    let current = r#"{"version": 4, "sources": {
        "openai": {"gpt-4.1": {"rates": {"input": 2.0, "output": 8.0}}}
    }}"#;
    assert_eq!(
        distill(current, &all_first_party(current))
            .expect("distill ok")
            .len(),
        1
    );
    let got = lines.snapshot();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got[0].contains("version 99") && got[0].contains("parsing best-effort"),
        "{got:?}"
    );
    assert!(got[1].contains("version missing"), "{got:?}");
}

// ── id resolution ────────────────────────────────────────────────────────────

#[test]
fn ids_match_case_insensitively() {
    let t = table(vec![eq_model("gpt-5.6-luna", 1e-6, 6e-6)]);
    assert_eq!(
        t.rate_at("gpt-5.6-luna", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
    assert_eq!(
        t.rate_at("GPT-5.6-LUNA", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
    assert!(t.rate_at("gpt-5.6", "2026-08-19", 0).is_none());
}

#[test]
fn rate_strips_bracket_suffix_before_match() {
    // A real model id prices the bracketed context ids.
    let ds = eq_model("deepseek-v4-pro", 4.35e-7, 8.7e-7);
    // An exact match must NOT match a non-context bracket, so the strip has
    // to be selective (digits + k/m only).
    let glm = eq_model("glm-5.2", 1.4e-6, 4.4e-6);
    let t = table(vec![ds, glm]);

    for id in [
        "deepseek-v4-pro[1m]",
        "deepseek-v4-pro[64k]",
        "deepseek-v4-pro[1M]",
        "deepseek-v4-pro[64K]",
        "glm-5.2[1m]",
    ] {
        assert!(t.rate_at(id, "2026-08-19", 0).is_some(), "{id}");
    }
    // Non-context brackets are left alone → the full id misses the row.
    assert!(t.rate_at("glm-5.2[xm]", "2026-08-19", 0).is_none());
    assert!(t.rate_at("glm-5.2[1x]", "2026-08-19", 0).is_none());
    assert!(t.rate_at("glm-5.2[]", "2026-08-19", 0).is_none());
}

// ── resolution retries ───────────────────────────────────────────────────────

#[test]
fn rate_retries_colon_strip_bare() {
    let t = table(vec![eq_model("glm-4.7", 1.4e-6, 4.4e-6)]);
    assert_eq!(
        t.rate_at("glm-4.7:free", "2026-08-19", 0).map(|r| r.input),
        Some(1.4e-6)
    );
}

#[test]
fn rate_retries_colon_strip_namespaced() {
    // The colon-stripped form still carries its namespace and must match a
    // row on that full form.
    let t = table(vec![eq_model("zai/glm-4.7", 1.4e-6, 4.4e-6)]);
    assert_eq!(
        t.rate_at("zai/glm-4.7:free", "2026-08-19", 0)
            .map(|r| r.input),
        Some(1.4e-6)
    );
}

#[test]
fn rate_retries_provider_strip_one_segment() {
    let t = table(vec![eq_model("claude-opus-5", 5e-6, 25e-6)]);
    assert_eq!(
        t.rate_at("anthropic/claude-opus-5", "2026-08-19", 0)
            .map(|r| r.input),
        Some(5e-6)
    );
    // The bracket strip fires on the ORIGINAL form, so a bracketed
    // namespaced id retries from its bracket-stripped self.
    assert_eq!(
        t.rate_at("anthropic/claude-opus-5[1m]", "2026-08-19", 0)
            .map(|r| r.input),
        Some(5e-6)
    );
}

#[test]
fn rate_retries_provider_strip_two_segments() {
    // Each intermediate retries: with a row on the ONE-segment form,
    // that form must win before the bare id is ever tried.
    let one_segment = eq_model("anthropic/claude-opus-5", 5e-6, 25e-6);
    let bare = eq_model("claude-opus-5", 6e-6, 26e-6);
    let t = table(vec![one_segment, bare]);
    assert_eq!(
        t.rate_at("openrouter/anthropic/claude-opus-5", "2026-08-19", 0)
            .map(|r| r.input),
        Some(5e-6)
    );
    // And when the intermediate has no row, the second strip prices.
    let t2 = table(vec![eq_model("claude-opus-5", 6e-6, 26e-6)]);
    assert_eq!(
        t2.rate_at("openrouter/anthropic/claude-opus-5", "2026-08-19", 0)
            .map(|r| r.input),
        Some(6e-6)
    );
}

#[test]
fn rate_retries_date_stamp_strip() {
    let t = table(vec![eq_model("glm-4.7-flash", 1e-6, 2e-6)]);
    assert_eq!(
        t.rate_at("glm-4.7-flash-20250801", "2026-08-19", 0)
            .map(|r| r.input),
        Some(1e-6)
    );
    // Only a trailing group of EXACTLY 8 digits is a date stamp; a shorter
    // (even all-numeric) or non-numeric group is a variant name and must
    // not strip.
    assert!(t.rate_at("glm-4.7-flash-250801", "2026-08-19", 0).is_none());
    assert!(t.rate_at("glm-4.7-flash-2508", "2026-08-19", 0).is_none());
    assert!(t.rate_at("glm-4.7-flash-fast", "2026-08-19", 0).is_none());
}

#[test]
fn rate_retries_date_stamp_strip_repeated() {
    // `m-20250801-20250802` strips BOTH trailing groups, retrying each
    // intermediate; only the fully-stripped `m` has a row here.
    let t = table(vec![eq_model("m", 1e-6, 2e-6)]);
    assert_eq!(
        t.rate_at("m-20250801-20250802", "2026-08-19", 0)
            .map(|r| r.input),
        Some(1e-6)
    );
}

#[test]
fn rate_retries_colon_before_provider_strip() {
    // `x/y:z` colon-strips to `x/y` AND (if reached) provider-strips to
    // `y`; the colon-stripped form must resolve first.
    let colon_form = eq_model("x/y", 1e-6, 2e-6);
    let provider_form = eq_model("y", 3e-6, 4e-6);
    let t = table(vec![colon_form, provider_form]);
    assert_eq!(
        t.rate_at("x/y:z", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
}

#[test]
fn rate_bracket_strip_applies_to_original_only() {
    // The bracket strip is part of the PRIMARY form; a retried form is not
    // re-stripped, so `m[1k]:x` colon-strips to `m[1k]` and misses `m`.
    let t = table(vec![eq_model("m", 1e-6, 2e-6)]);
    assert!(t.rate_at("m[1k]:x", "2026-08-19", 0).is_none());
}

#[test]
fn rate_retries_propagate_date_and_hour() {
    // The retry ladder must hand its (date, hour) through to entry selection
    // unchanged: an id that matches ONLY after a strip still prices
    // peak/off-peak and effective-gated rows by the queried (date, hour).
    let m = PricedModel {
        id: "m".to_owned(),
        prices: vec![
            entry(0.2175e-6, 0.435e-6), // off-peak fallback
            PriceEntry {
                input: 0.435e-6,
                output: 0.87e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "01:00".to_owned(),
                    end: "04:00".to_owned(),
                }),
            },
        ],
        effective_at: None,
    };
    let t = table(vec![m]);
    // `m:free` matches only via the colon strip, so the hour used comes
    // from the RETRY call, not the primary walk.
    let input = |hour: u8| t.rate_at("m:free", "2026-08-19", hour).map(|r| r.input);
    assert_eq!(input(2), Some(0.435e-6)); // peak
    assert_eq!(input(4), Some(0.2175e-6)); // off-peak (04:00 half-open)

    // Effective-gate propagation: the fixture's windowed deepseek-v4-pro row
    // is effective from 2026-08-23; a colon-stripped id respects the gate.
    let ft = PriceTable::capture(
        distill(FIXTURE, &fixture_guard()).expect("fixture distills"),
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    let ds = |date: &str| ft.rate_at("deepseek-v4-pro:free", date, 5).map(|r| r.input);
    assert_eq!(ds("2026-08-19"), None); // before the effective date: nothing
    assert_rate(ds("2026-08-28"), 6.6e-7); // after: base rate
}

// ── constraint resolution ────────────────────────────────────────────────────

#[test]
fn every_weekday_name_matches_the_feeds_spelling() {
    // A `Constraint::Days` compares this name against the feed's `days` set by
    // string, so one wrong arm silently unpeaks (or peaks) a whole weekday.
    // 2026-08-24..30 is one Monday-to-Sunday week, so all seven arms answer.
    let week = [
        ("2026-08-24", "monday"),
        ("2026-08-25", "tuesday"),
        ("2026-08-26", "wednesday"),
        ("2026-08-27", "thursday"),
        ("2026-08-28", "friday"),
        ("2026-08-29", "saturday"),
        ("2026-08-30", "sunday"),
    ];
    for (date, name) in week {
        assert_eq!(date_weekday(date), Some(name), "{date}");
    }
    // And behaviourally, through the selection the names exist for: an entry
    // constrained to one weekday is active on that day alone.
    for (date, name) in week {
        let m = PricedModel {
            id: "m".to_owned(),
            prices: vec![
                entry(1e-6, 2e-6),
                PriceEntry {
                    input: 3e-6,
                    output: 4e-6,
                    cache_read: 0.0,
                    cache_write: 0.0,
                    constraint: Some(Constraint::Days {
                        days: vec![name.to_owned()],
                        start: None,
                        end: None,
                    }),
                },
            ],
            effective_at: None,
        };
        let t = table(vec![m]);
        for (other, _) in week {
            let want = if other == date { 3e-6 } else { 1e-6 };
            assert_rate(t.rate_at("m", other, 12).map(|r| r.input), want);
        }
    }
    // An unparseable date names no weekday, so the constraint is never active
    // and selection falls through to the base entry.
    assert_eq!(date_weekday("not-a-date"), None);
    assert_eq!(date_weekday("2026-13-40"), None);
}

#[test]
fn time_window_hour_granularity_boundaries() {
    // deepseek-chat's real V3-era window: peak 00:30–16:30Z, off-peak else.
    // At hour granularity `(start_h,start_m) <= (h,0) < (end_h,end_m)`:
    // hour 0 is off-peak (00:00 < 00:30) and hour 16 is PEAK (16:00 < 16:30);
    // the :30 boundaries are half-mispriced by construction (documented).
    let chat = PricedModel {
        id: "deepseek-chat".to_owned(),
        prices: vec![
            entry(0.135e-6, 0.55e-6),
            PriceEntry {
                input: 0.27e-6,
                output: 1.1e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "00:30:00Z".to_owned(),
                    end: "16:30:00Z".to_owned(),
                }),
            },
        ],
        effective_at: None,
    };
    let t = table(vec![chat]);
    let input = |hour: u8| {
        t.rate_at("deepseek-chat", "2026-08-19", hour)
            .map(|r| r.input)
    };
    assert_eq!(input(0), Some(0.135e-6)); // off-peak
    assert_eq!(input(5), Some(0.27e-6)); // peak
    assert_eq!(input(16), Some(0.27e-6)); // peak: (16,0) < (16,30)
    assert_eq!(input(17), Some(0.135e-6)); // off-peak again
    assert_eq!(input(23), Some(0.135e-6)); // off-peak
}

#[test]
fn two_window_peak_offpeak_peak() {
    // Reversed-entry selection tries the 06:00 window first, then 01:00, then
    // the unconstrained off-peak fallback.
    let t = table(vec![two_window_model()]);
    let input = |hour: u8| {
        t.rate_at("deepseek-v4-pro", "2026-08-19", hour)
            .map(|r| r.input)
    };
    assert_eq!(input(1), Some(0.435e-6)); // peak, first window
    assert_eq!(input(4), Some(0.2175e-6)); // 04:00 is excluded (half-open)
    assert_eq!(input(5), Some(0.2175e-6)); // gap between windows
    assert_eq!(input(7), Some(0.435e-6)); // peak, second window
    assert_eq!(input(10), Some(0.2175e-6)); // 10:00 excluded
    assert_eq!(input(23), Some(0.2175e-6)); // off-peak
}

#[test]
fn no_active_entry_prices_nothing() {
    // The old resolver served `prices[0]` when nothing matched — the leak
    // that would have priced a row before its effective date. A model whose
    // entries are ALL inactive now prices nothing.
    let m = PricedModel {
        id: "windowed-only".to_owned(),
        prices: vec![
            PriceEntry {
                input: 3e-6,
                output: 4e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "01:00".to_owned(),
                    end: "04:00".to_owned(),
                }),
            },
            PriceEntry {
                input: 9e-6,
                output: 10e-6,
                cache_read: 0.0,
                cache_write: 0.0,
                constraint: Some(Constraint::TimeWindow {
                    start: "06:00".to_owned(),
                    end: "10:00".to_owned(),
                }),
            },
        ],
        effective_at: None,
    };
    let t = table(vec![m]);
    assert!(t.rate_at("windowed-only", "2026-08-19", 5).is_none());
    assert!(
        t.rate_at("windowed-only", "2026-08-19", 2).is_some(),
        "an active window still prices"
    );
}

// ── effective_at gating ──────────────────────────────────────────────────────

#[test]
fn effective_at_gates_the_whole_row() {
    // The fixture's real deepseek-v4-pro row: effective 2026-08-23, weekday
    // peak windows 01:00-04:00 and 06:00-10:00 UTC. A query before the date
    // prices NOTHING — the gate covers the window entries, not only the base.
    let t = PriceTable::capture(
        distill(FIXTURE, &fixture_guard()).expect("fixture distills"),
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    let input = |date: &str, hour: u8| t.rate_at("deepseek-v4-pro", date, hour).map(|r| r.input);
    // 2026-08-19 is a Wednesday: the 01:00 window would match hour 2 — but
    // the row is not yet effective.
    assert_eq!(input("2026-08-19", 2), None);
    // The effective date itself prices: 2026-08-23 is a Sunday, so hour 2
    // is base rate.
    assert_rate(input("2026-08-23", 2), 6.6e-7);
    // 2026-08-28 is a Friday: peak hours price the window entries.
    assert_rate(input("2026-08-28", 2), 1.32e-6);
    assert_rate(input("2026-08-28", 5), 6.6e-7);
    assert_rate(input("2026-08-28", 8), 1.32e-6);
    assert_rate(input("2026-08-28", 23), 6.6e-7);
    // 2026-08-30 is a Sunday: the weekday windows do not apply.
    assert_rate(input("2026-08-30", 8), 6.6e-7);
}

#[test]
fn future_effective_row_prices_nothing_until_its_date() {
    // The fixture's synthetic future-effective row (no live row exercises
    // this): flat base rates apply from 2026-12-31 on.
    let t = PriceTable::capture(
        distill(FIXTURE, &fixture_guard()).expect("fixture distills"),
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    let input = |date: &str| {
        t.rate_at("synth-future-effective", date, 0)
            .map(|r| r.input)
    };
    assert_eq!(input("2026-12-30"), None);
    assert_rate(input("2026-12-31"), 2.5e-7);
    assert_rate(input("2027-01-01"), 2.5e-7);
}

// ── window overlap precedence ────────────────────────────────────────────────

#[test]
fn later_window_entry_wins_when_both_match() {
    // The fixture's synthetic overlap row (no live kept-source pair overlaps;
    // the zai glm-5.3-flash canonical case is quota-only and skipped at
    // distill, so it cannot serve as the pin): a weekday whole-day entry,
    // then a weekday 08:00-20:00 entry. Friday hour 10 matches BOTH — the
    // later entry wins.
    let t = PriceTable::capture(
        distill(FIXTURE, &fixture_guard()).expect("fixture distills"),
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    let rate = |date: &str, hour: u8| t.rate_at("synth-overlap", date, hour).expect("priced");
    assert!(
        (rate("2026-08-28", 10).input - 5e-7).abs() < 1e-12,
        "later entry wins"
    );
    assert!(
        (rate("2026-08-28", 5).input - 3e-7).abs() < 1e-12,
        "only the whole-day entry"
    );
    assert!(
        (rate("2026-08-28", 10).output - 2e-7).abs() < 1e-12,
        "output inherits base"
    );
    assert!(
        (rate("2026-08-30", 10).input - 1e-7).abs() < 1e-12,
        "Sunday: base rate"
    );
}

// ── snapshot history ─────────────────────────────────────────────────────────

#[test]
fn snapshot_picks_rate_live_on_date() {
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let t = PriceTable {
        models: history[1].models.clone(),
        history,
        store: Vec::new(),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    let input = |date: &str| t.rate_at("m", date, 0).map(|r| r.input);
    assert_eq!(input("2026-05-31"), Some(1e-6)); // day before the change
    assert_eq!(input("2026-06-01"), Some(2e-6)); // the change date itself
    assert_eq!(input("2026-09-01"), Some(2e-6)); // after
}

#[test]
fn date_before_first_snapshot_uses_first() {
    let t = table(vec![eq_model("m", 1e-6, 2e-6)]);
    assert_eq!(t.rate_at("m", "2025-01-01", 0).map(|r| r.input), Some(1e-6));
}

#[test]
fn capture_appends_only_on_change() {
    let t = PriceTable::capture(
        vec![eq_model("m", 1e-6, 2e-6)],
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-19".to_owned(),
        42,
        Vec::new(),
    );
    assert_eq!(t.history.len(), 1);

    // Identical refetch: same models → no new snapshot, capture date dropped.
    let t2 = PriceTable::capture(
        vec![eq_model("m", 1e-6, 2e-6)],
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-20".to_owned(),
        43,
        t.history.clone(),
    );
    assert_eq!(t2.history.len(), 1);
    assert_eq!(t2.history[0].captured, "2026-08-19");

    // A rate change appends.
    let t3 = PriceTable::capture(
        vec![eq_model("m", 2e-6, 4e-6)],
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-20".to_owned(),
        44,
        t2.history.clone(),
    );
    assert_eq!(t3.history.len(), 2);
    assert_eq!(t3.history[1].captured, "2026-08-20");
    // The working set is the newest snapshot's models.
    assert_eq!(
        t3.rate_at("m", "2026-08-20", 0).map(|r| r.input),
        Some(2e-6)
    );
}

#[test]
fn capture_caps_history_at_180() {
    let mut t = PriceTable::capture(
        vec![eq_model("m", 0.0, 0.0)],
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-01-01".to_owned(),
        0,
        Vec::new(),
    );
    for i in 1..=182u32 {
        let rate = f64::from(i) * 1e-6;
        t = PriceTable::capture(
            vec![eq_model("m", rate, 0.0)],
            Vec::new(),
            Vec::new(),
            CanonicalMap::default(),
            format!("2026-{i:03}"),
            u64::from(i),
            t.history,
        );
    }
    assert_eq!(t.history.len(), HISTORY_CAP);
    // 183 appends total: the three oldest (rates 0, 1, 2) dropped.
    assert!(
        (t.history[0].models[0].prices[0].input - 3e-6).abs() < 1e-12,
        "got {}",
        t.history[0].models[0].prices[0].input
    );
    assert!(
        (t.history[HISTORY_CAP - 1].models[0].prices[0].input - 182e-6).abs() < 1e-9,
        "got {}",
        t.history[HISTORY_CAP - 1].models[0].prices[0].input
    );
}

#[test]
fn cache_round_trip_preserves_history() {
    let sandbox = HomeSandbox::new();
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let table = PriceTable {
        models: history[1].models.clone(),
        history,
        store: Vec::new(),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 12345,
        memo: Mutex::default(),
    };
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_v4_price_cache.json");
    save_cache(&path, &table);

    let loaded = load_cached().expect("cache loads");
    assert_eq!(loaded.fetched_at_ms, 12345);
    assert_eq!(loaded.history.len(), 2);
    assert_eq!(
        loaded.rate_at("m", "2026-08-19", 0).map(|r| r.input),
        Some(2e-6)
    );
    assert_eq!(
        loaded.rate_at("m", "2026-02-01", 0).map(|r| r.input),
        Some(1e-6)
    );
}

#[test]
fn load_cache_rejects_empty_history() {
    let sandbox = HomeSandbox::new();
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_v4_price_cache.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, r#"{"fetched_at_ms": 1, "history": []}"#).expect("write");
    assert!(load_cached().is_none());
}

#[test]
fn stale_caches_deleted_once() {
    // Every superseded cache file goes in one cleanup pass after the first
    // successful new-cache write; the flag is set before the deletes, so a
    // reappearing file is never re-deleted. The v3-feed name joins the two
    // pre-ai-pricelog ones: its `store` was filtered by the retired provider
    // allowlist, so a v4 build reading it would serve the rows this change
    // drops for as long as `first_delay` holds off the first fetch.
    let sandbox = HomeSandbox::new();
    let dir = sandbox.home().join(".clauth");
    let new_path = dir.join("ai_pricelog_v4_price_cache.json");
    let stale: Vec<PathBuf> = [
        "price_cache.json",
        "genai_price_cache.json",
        "ai_pricelog_price_cache.json",
    ]
    .iter()
    .map(|name| dir.join(name))
    .collect();
    let write_all = || {
        std::fs::create_dir_all(&dir).expect("mkdir");
        for path in &stale {
            std::fs::write(path, "{}").expect("write");
        }
    };
    write_all();

    let mut done = false;
    delete_stale_cache_once(&new_path, &mut done);
    assert!(done);
    for path in &stale {
        assert!(!path.exists(), "{}", path.display());
    }

    write_all();
    delete_stale_cache_once(&new_path, &mut done);
    for path in &stale {
        assert!(path.exists(), "{}", path.display());
    }
}

#[test]
fn the_v3_cache_name_is_not_the_one_this_build_reads() {
    // The upgrade path: a v3-era cache sitting in place must NOT prime this
    // build. Its bytes parse — the cached types never moved — so nothing but
    // the filename separates "loads the old guard's rows" from "starts cold",
    // and `first_delay` would hold the first v4 fetch off by the interval
    // minus the cache's age.
    let sandbox = HomeSandbox::new();
    let dir = sandbox.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let old = dir.join("ai_pricelog_price_cache.json");
    std::fs::write(
        &old,
        r#"{"fetched_at_ms": 7, "history": [{"captured": "2026-01-01", "models": [{"id": "m", "prices": [{"input": 1e-6, "output": 2e-6, "cache_read": 0.0, "cache_write": 0.0, "constraint": null}], "effective_at": null}]}]}"#,
    )
    .expect("write");
    assert!(
        load_cached().is_none(),
        "the v3 cache name must not prime a v4 build"
    );
    // A cold cache is what `first_delay` ticks immediately, so the upgrade
    // fetches on the next start rather than up to an interval later.
    assert_eq!(
        first_delay(None, 1_000_000, REFRESH_INTERVAL),
        Duration::ZERO
    );
    // Written under the new name, the same bytes do prime it.
    std::fs::rename(&old, dir.join("ai_pricelog_v4_price_cache.json")).expect("rename");
    assert_eq!(load_cached().map(|t| t.fetched_at_ms), Some(7));
}

// ── real-index fixture ───────────────────────────────────────────────────────

#[test]
fn fixture_distills_resolvers_and_excludes_resellers() {
    let models = distill(FIXTURE, &fixture_guard()).expect("fixture distills");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    // The vendorless reseller's source is dropped row by row, so nothing of
    // it survives.
    assert!(!ids.contains(&"aion-labs/aion-2.0"));
    // The first-party representatives survive.
    for id in [
        "claude-opus-5",
        "gpt-4.1",
        "qwen3.8-max",
        "glm-5.3-flash",
        "kimi-k2.6",
        "grok-4.6",
        "deepseek-v4-pro",
        "synth-future-effective",
        "synth-overlap",
    ] {
        assert!(ids.contains(&id), "{id} missing");
    }
    // dashscope's six resold rows drop on the vendor comparison, its own qwen
    // row beside them survives.
    for id in [
        "deepseek-v3.2",
        "deepseek-v4-flash",
        "glm-5.1",
        "glm-5.2",
        "kimi-k2.7-code",
    ] {
        assert!(!ids.contains(&id), "resold row {id} must drop");
    }
    // deepseek's own deepseek-r1 row carries `removed_at`: it is first-party,
    // so only the delisting can drop it — and the row above it from the same
    // source shows the guard kept that source's rows.
    assert!(
        !ids.contains(&"deepseek-r1"),
        "a first-party delisted row must drop too"
    );
    assert!(
        distill(FIXTURE, &all_first_party(FIXTURE))
            .expect("fixture distills")
            .iter()
            .all(|m| m.id != "deepseek-r1"),
        "and it drops with no resold guard at all — the stamp is what dashes it"
    );

    let t = PriceTable::capture(
        models,
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    // One claude-* row prices.
    let claude = t
        .rate_at("claude-opus-5", "2026-08-30", 0)
        .expect("claude row prices");
    assert!((claude.input - 5e-6).abs() < 1e-12);
    assert!((claude.output - 25e-6).abs() < 1e-12);
    assert!((claude.cache_read - 5e-7).abs() < 1e-12);
    // The 1-hour cache-write field stays unused: the 5-minute rate wins.
    assert!((claude.cache_write - 6.25e-6).abs() < 1e-12);
    // One gpt-* row prices.
    let gpt = t
        .rate_at("gpt-4.1", "2026-08-30", 0)
        .expect("gpt row prices");
    assert!((gpt.input - 2e-6).abs() < 1e-12);
    assert!((gpt.output - 8e-6).abs() < 1e-12);
    // One qwen-* row prices.
    let qwen = t
        .rate_at("qwen3.8-max", "2026-08-30", 0)
        .expect("qwen row prices");
    assert!((qwen.input - 2e-6).abs() < 1e-12);
    assert!((qwen.output - 6e-6).abs() < 1e-12);

    // The ruling's regression guard: deepseek-v4-pro prices deepseek's OWN
    // row and never the dashscope resold copy — the dashscope row carries
    // `removed_at` and is delisted at distill, so exactly one distilled model
    // carries the id, and its rates are deepseek's own.
    let ds_models: Vec<&PricedModel> = t
        .models
        .iter()
        .filter(|m| m.id.eq_ignore_ascii_case("deepseek-v4-pro"))
        .collect();
    assert_eq!(ds_models.len(), 1, "the delisted copy must not shadow");
    assert_eq!(ds_models[0].prices[0].constraint, None);
    assert!((ds_models[0].prices[0].input - 6.6e-7).abs() < 1e-12);
    assert!((ds_models[0].prices[0].output - 1.98e-6).abs() < 1e-12);
    // Bracket-stripped ids resolve through the same row.
    assert_rate(
        t.rate_at("deepseek-v4-pro[1m]", "2026-08-28", 5)
            .map(|r| r.input),
        6.6e-7,
    );
}

// ── cost ─────────────────────────────────────────────────────────────────────

#[test]
fn cost_sums_all_four_buckets() {
    // Clean rates: $1/$2/$0.10/$1.25 per million.
    let t = table(vec![PricedModel {
        id: "m".to_owned(),
        prices: vec![PriceEntry {
            input: 1e-6,
            output: 2e-6,
            cache_read: 1e-7,
            cache_write: 1.25e-6,
            constraint: None,
        }],
        effective_at: None,
    }]);
    let m = model("m", 1_000_000, 1_000_000, 1_000_000, 1_000_000);
    // 1.0 + 2.0 + 0.10 + 1.25 = 4.35
    let c = t.cost_at(&m, "2026-08-19", 0).expect("priced");
    assert!((c - 4.35).abs() < 1e-9, "got {c}");
}

#[test]
fn cost_none_for_unpriced_model() {
    let t = table(vec![PricedModel {
        id: "m".to_owned(),
        prices: vec![PriceEntry {
            input: 1e-6,
            output: 2e-6,
            cache_read: 1e-7,
            cache_write: 1.25e-6,
            constraint: None,
        }],
        effective_at: None,
    }]);
    assert!(
        t.cost_at(&model("unknown", 1000, 0, 0, 0), "2026-08-19", 0)
            .is_none()
    );
}

#[test]
fn cost_at_uses_dated_rate() {
    // A table with two snapshots: cost_at follows the date's rate.
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let t = PriceTable {
        models: history[1].models.clone(),
        history,
        store: Vec::new(),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    let m = model("m", 1_000_000, 0, 0, 0);
    assert!((t.cost_at(&m, "2026-05-01", 0).expect("priced") - 1.0).abs() < 1e-9);
    assert!((t.cost_at(&m, "2026-06-01", 0).expect("priced") - 2.0).abs() < 1e-9);
}

#[test]
fn cost_day_prices_each_hour_at_its_rate() {
    // Peak hours 1-3 and 6-9 (7 hours) at $0.435/M, the other 17 at half.
    let t = table(vec![two_window_model()]);
    let mut hours = [HourTokens::default(); 24];
    for h in &mut hours {
        h.input = 1_000_000;
    }
    let cost = t
        .cost_day("deepseek-v4-pro", "2026-08-19", &hours)
        .expect("priced");
    let expected = 7.0 * 0.435 + 17.0 * 0.2175;
    assert!((cost - expected).abs() < 1e-9, "got {cost}");

    // An unpriced model is None even with tokens on the clock.
    assert!(t.cost_day("unknown", "2026-08-19", &hours).is_none());
}

// ── memo ─────────────────────────────────────────────────────────────────────

#[test]
fn the_cost_lens_walks_each_model_and_date_once() {
    // The weekly lens' shape: `render::tokens::day_cost_split` prices each of a
    // day's 24 hours through `rate_at`, for every model, every day, on every
    // frame. The match walk is hour-independent, so a week of that is one walk
    // per (model, date) however many hours and frames ask for it.
    let models = || vec![two_window_model(), eq_model("m", 1e-6, 2e-6)];
    let ids = ["deepseek-v4-pro", "m"];
    let week: Vec<String> = (19..=25).map(|d| format!("2026-08-{d}")).collect();
    let frame = |t: &PriceTable| -> f64 {
        let mut usd = 0.0;
        for date in &week {
            for id in ids {
                for hour in 0..24u8 {
                    usd += t.rate_at(id, date, hour).expect("priced").input;
                }
            }
        }
        usd
    };

    let t = table(models());
    let first = frame(&t);
    assert_eq!(t.walks(), 14, "one walk per (model, date), never per hour");
    let second = frame(&t);
    assert_eq!(t.walks(), 14, "and the next frame walks nothing at all");
    assert!((first - second).abs() < 1e-15, "{first} vs {second}");

    // The figures a table that remembers nothing produces, one query at a time.
    let mut cold = 0.0;
    for date in &week {
        for id in ids {
            for hour in 0..24u8 {
                cold += table(models())
                    .rate_at(id, date, hour)
                    .expect("priced")
                    .input;
            }
        }
    }
    assert!((first - cold).abs() < 1e-15, "{first} vs {cold}");

    // And `cost_day`, which resolves the model once and prices 24 hours off it,
    // lands on the same total as 24 separate `rate_at` calls.
    let mut hours = [HourTokens::default(); 24];
    for h in &mut hours {
        h.input = 1_000_000;
    }
    let mut day = 0.0;
    let mut per_hour = 0.0;
    for id in ids {
        day += t.cost_day(id, &week[0], &hours).expect("priced");
        for hour in 0..24u8 {
            per_hour += t.rate_at(id, &week[0], hour).expect("priced").input * 1_000_000.0;
        }
    }
    assert!((day - per_hour).abs() < 1e-9, "{day} vs {per_hour}");
}

#[test]
fn an_unpriced_id_is_remembered_as_unpriced() {
    // A miss costs the FULL ladder — every candidate form against every model —
    // so it is the walk least worth repeating. Unpriced ids reach the lens on
    // every frame too: a local fine-tune the feed carries no rate for.
    let t = table(vec![eq_model("m", 1e-6, 2e-6)]);
    assert!(t.rate_at("qwable-9b", "2026-08-19", 0).is_none());
    assert_eq!(t.walks(), 1);
    assert!(t.rate_at("qwable-9b", "2026-08-19", 7).is_none());
    assert_eq!(t.walks(), 1, "the miss is remembered, not re-walked");
}

#[test]
fn the_memo_keys_on_the_date_so_two_snapshots_do_not_bleed() {
    // The same id sits at a different offset in each era's snapshot. Remembering
    // it by id alone would price the June query at January's offset, which here
    // is a different model at 9× the rate.
    let history = vec![
        RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        },
        RateSnapshot {
            captured: "2026-06-01".to_owned(),
            models: vec![eq_model("other", 9e-6, 9e-6), eq_model("m", 2e-6, 4e-6)],
        },
    ];
    let t = PriceTable {
        models: history[1].models.clone(),
        history,
        store: Vec::new(),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    let input = |date: &str| t.rate_at("m", date, 0).map(|r| r.input);
    assert_eq!(input("2026-05-31"), Some(1e-6));
    assert_eq!(input("2026-06-01"), Some(2e-6));
    assert_eq!(
        t.walks(),
        2,
        "one walk per date, since the snapshots differ"
    );
}

// ── the zai quota rows ───────────────────────────────────────────────────────

#[test]
fn zai_quota_entries_are_skipped() {
    // The fixture's real glm-5.3-flash row: quota_multiplier-only window
    // entries distill to nothing, so a weekday peak hour prices the flat base.
    let models = distill(FIXTURE, &fixture_guard()).expect("fixture distills");
    let flash = models
        .iter()
        .find(|m| m.id == "glm-5.3-flash")
        .expect("row distills");
    assert_eq!(flash.prices.len(), 1, "quota entries contribute no rates");
    let t = PriceTable::capture(
        models,
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    let rate = t.rate_at("glm-5.3-flash", "2026-08-28", 8).expect("priced");
    assert!(
        (rate.input - 7.5e-8).abs() < 1e-15,
        "no peak multiplier applies"
    );
    assert!((rate.output - 2.5e-7).abs() < 1e-15);
}

// ── delisted rows ────────────────────────────────────────────────────────────

#[test]
fn removed_at_stamped_entries_price_nothing() {
    // The index's removal convention keeps a row with its last prices and
    // stamps it `removed_at`; the stamp is what makes a delisting effective
    // in clauth. deepseek's own deepseek-r1 row is the pin that isolates the
    // stamp — it is FIRST-PARTY, so the resold guard cannot be what dashes
    // it. The dashscope ids beside it drop for the other reason and stay
    // unpriced too, having no live twin.
    let t = PriceTable::capture(
        distill(FIXTURE, &fixture_guard()).expect("fixture distills"),
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    for id in [
        "deepseek-r1",
        "deepseek-v3.2",
        "deepseek-v4-flash",
        "glm-5.1",
        "glm-5.2",
        "kimi-k2.7-code",
    ] {
        for date in ["2026-01-01", "2026-08-30", "2027-01-01"] {
            for hour in [0, 12, 23] {
                assert!(
                    t.rate_at(id, date, hour).is_none(),
                    "delisted {id} priced at {date} hour {hour}"
                );
            }
        }
    }
}

// ── store-history dating ──────────────────────────────────────────────────────

/// Trimmed real ai-pricelog v4 history
/// (`tests/fixtures/ai-pricelog-history-trimmed.ndjson`): five of deepseek's
/// deepseek-v4-pro rows (the 2026-08-30 retro-effective windowed row whose
/// `when`s name a timezone, the 2026-08-26 whole-week windowed row, the
/// 08-16 / 05-22 / 04-24 flat rows), deepseek-v4-flash's 2026-04-24 row (the
/// alias chain's priced canonical) plus its 2026-08-26 windowed row (the
/// fixture's WINNING windowed row — the per-hour pin; the full store shadows
/// it with an 08-30 row, which the trim drops), grok-4.5's 2026-07-09 row
/// (the null-bound chain's canonical), minimax's MiniMax-M2.5-Lightning
/// price and bare-removal pair, two of together's four deepseek-v4-pro rows
/// (the 06-09 markup and the 08-28 removal), one of avian's two (the 05-04
/// markup), cloudflare's one row of its forty
/// (`@cf/deepseek-ai/deepseek-v4-pro-0813`, the dropped source's id behind the
/// reseller-dash pin), openrouter's `fees`-bearing claude-3-haiku row (it
/// parses, and the resold guard keeps it out of the walk), dashscope's
/// resold deepseek-v3.2 price and removal pair beside its OWN qwen3.8-max row
/// (the mixed-provider split), deepseek-r1's and deepseek-v3's 2025 rows (the
/// alias canonicals), deepseek-r1's 2026-08-31 removal row — a FIRST-PARTY
/// removal that CARRIES rates, the shape 15 keys of the real store hold and
/// the only one that can tell a live terminator from a rate-less row that
/// dashes on its own — and zai's glm-4.5 same-day pair (a kept-key tie group
/// that differs in rates at its key's newest observed day). Every line is
/// verbatim store bytes.
const HISTORY_FIXTURE: &str = include_str!("../fixtures/ai-pricelog-history-trimmed.ndjson");

/// A table whose only dating source is the history fixture — no snapshot log,
/// so every date resolves through the store walk.
fn store_table() -> PriceTable {
    PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills"),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    }
}

#[test]
fn history_distills_first_party_keys_only() {
    let keys = distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills");
    let ids: Vec<&str> = keys.iter().map(|k| k.id.as_str()).collect();
    // First-seen store order. together / avian / cloudflare / openrouter
    // publish no vendor, and dashscope's deepseek-v3.2 pair is a resale, so
    // none of their rows enters: they can neither price an id nor delist it.
    // dashscope's OWN qwen row does enter, one row after the resale it lost.
    assert_eq!(
        ids,
        [
            "MiniMax-M2.5-Lightning",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "grok-4.5",
            "glm-4.5",
            "qwen3.8-max",
            "deepseek-r1",
            "deepseek-v3",
            "deepseek-v3.2"
        ]
    );
    // Each surviving key names the source that MAKES it.
    assert!(
        keys.iter().all(|k| matches!(
            (k.source.as_str(), k.id.as_str()),
            ("deepseek", _)
                | ("minimax", _)
                | ("xai", _)
                | ("zai", _)
                | ("dashscope", "qwen3.8-max")
        )),
        "{:?}",
        keys.iter()
            .map(|k| (k.source.as_str(), k.id.as_str()))
            .collect::<Vec<_>>()
    );
    let ds = keys
        .iter()
        .find(|k| k.id == "deepseek-v4-pro")
        .expect("deepseek key");
    assert_eq!(ds.rows.len(), 5);
    // The bare removal row is kept as a terminator although it has no prices.
    let mm = keys
        .iter()
        .find(|k| k.id == "MiniMax-M2.5-Lightning")
        .expect("minimax key");
    assert_eq!(mm.rows.len(), 2);
    assert!(mm.rows[1].removed && mm.rows[1].model.is_none());

    // Tolerance: malformed lines skip beside a good one; zero keys fail.
    let mixed = concat!(
        r#"{"schema":4,"source":"deepseek","model_id":"m","observed_at":"2026-01-01","#,
        r#""rates":{"input":1,"output":2}}"#,
        "\nnot json\n{}\n"
    );
    assert_eq!(
        distill_history(mixed, &all_first_party_history(mixed))
            .expect("one key")
            .len(),
        1
    );
    assert!(distill_history("", &FirstParty::default()).is_err());
    assert!(distill_history("not json at all", &FirstParty::default()).is_err());
}

#[test]
fn history_drops_a_mixed_providers_resales_and_keeps_its_own() {
    // dashscope's fixture rows are a resold deepseek-v3.2 price + removal
    // pair beside its own qwen3.8-max row. The resale never enters, so
    // deepseek-v3.2 prices deepseek's own rows at every date the store
    // covers — dashscope's 0.57 markup and its removal both unreachable —
    // while dashscope's qwen row prices normally.
    let keys = distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills");
    assert!(
        !keys
            .iter()
            .any(|k| k.source == "dashscope" && k.id == "deepseek-v3.2"),
        "the resale must not enter the walk at all"
    );
    let t = store_table();
    for date in ["2026-08-29", "2026-08-30", "2026-09-05"] {
        let rate = t.rate_at("deepseek-v3.2", date, 0).expect("priced");
        assert!(
            (rate.input - 2.8e-7).abs() < 1e-12,
            "{date}: {}",
            rate.input
        );
        assert!((rate.output - 4.2e-7).abs() < 1e-12, "{date}");
        assert!((rate.cache_read - 2.8e-8).abs() < 1e-15, "{date}");
    }
    assert_rate(
        t.rate_at("qwen3.8-max", "2026-08-30", 0).map(|r| r.input),
        2e-6,
    );
}

#[test]
fn store_walk_prices_the_weekday_peak_windows_per_hour() {
    // 2026-08-26 is a Wednesday inside the windowed row's retro span: the
    // 01:00–04:00 and 06:00–10:00 weekday windows price their hours at the
    // peak rate, every other hour at half.
    let t = store_table();
    let input = |hour: u8| {
        t.rate_at("deepseek-v4-pro", "2026-08-26", hour)
            .map(|r| r.input)
    };
    assert_rate(input(2), 1.32e-6); // first window
    assert_rate(input(3), 1.32e-6);
    assert_rate(input(4), 6.6e-7); // 04:00 excluded (half-open)
    assert_rate(input(5), 6.6e-7); // gap between windows
    assert_rate(input(7), 1.32e-6); // second window
    assert_rate(input(10), 6.6e-7); // 10:00 excluded
    assert_rate(input(23), 6.6e-7);
}

#[test]
fn a_winning_history_rows_windows_price_per_hour() {
    // A history row's `overrides` feed the same per-hour selection an index
    // row's do: deepseek-v4-flash's real 2026-08-26 row (two windows, no day
    // set, so every day peaks) is the fixture's NEWEST v4-flash row and wins
    // the walk for 08-26 on — the 08-30 row that shadows it in the full store
    // is trimmed out. Dropping the override loop in `row_entries` flattens
    // every hour to the base rate and reds this pin.
    let t = store_table();
    let input = |date: &str, hour: u8| {
        t.rate_at("deepseek-v4-flash", date, hour)
            .map(|r| (r.input, r.cache_read))
    };
    assert_rate(input("2026-08-27", 2).map(|r| r.0), 4.4e-7); // peak window
    assert_rate(input("2026-08-27", 5).map(|r| r.0), 2.2e-7); // off-peak
    assert_rate(input("2026-08-27", 2).map(|r| r.1), 1.4e-8); // peak cache-read
    // Before the legacy row's day, the 04-24 flat row still wins.
    assert_rate(input("2026-04-25", 2).map(|r| r.0), 1.4e-7);
}

#[test]
fn store_walk_prices_weekend_days_in_the_retro_span_off_peak() {
    // The windowed row's windows are weekday-only: 2026-08-23 (Sun), 08-29
    // (Sat) and 08-30 (Sun) price base at peak hours. An observed-at-only
    // walk would pick the 08-26 legacy `peak_windows` row instead — its
    // windows carry no day set, so every day peaks — and this is the pin
    // that reds there.
    let t = store_table();
    for date in ["2026-08-23", "2026-08-29", "2026-08-30"] {
        assert_rate(
            t.rate_at("deepseek-v4-pro", date, 2).map(|r| r.input),
            6.6e-7,
        );
    }
}

#[test]
fn store_walk_retro_dates_the_windowed_row_before_its_observation() {
    // The 2026-08-30 row applies from its effective_at (2026-08-23): the day
    // before prices the 08-16 row (no cache-read rate), the day itself prices
    // the windowed row's base — same input, the cache-read key flips.
    let t = store_table();
    let cache_read = |date: &str| t.rate_at("deepseek-v4-pro", date, 0).map(|r| r.cache_read);
    assert_eq!(cache_read("2026-08-22"), Some(0.0));
    assert_rate(cache_read("2026-08-23"), 2.2e-8);
    assert_rate(
        t.rate_at("deepseek-v4-pro", "2026-08-23", 0)
            .map(|r| r.input),
        6.6e-7,
    );
}

#[test]
fn store_walk_prices_each_side_of_a_reprice_at_its_own_rate() {
    // deepseek repriced 1.74 → 0.435 → 0.66 (2026-05-22, 2026-08-16): each
    // side of a reprice date prices at its own rate.
    let t = store_table();
    let input = |date: &str| t.rate_at("deepseek-v4-pro", date, 0).map(|r| r.input);
    assert_rate(input("2026-04-24"), 1.74e-6);
    assert_rate(input("2026-05-22"), 4.35e-7);
    assert_rate(input("2026-08-15"), 4.35e-7);
    assert_rate(input("2026-08-16"), 6.6e-7);
}

#[test]
fn store_walk_dash_before_the_oldest_row() {
    // A day before the store's oldest row for a key prices nothing:
    // pre-install days price from the store's own rows and dash only where
    // the store has none.
    let t = store_table();
    assert!(t.rate_at("deepseek-v4-pro", "2026-04-23", 5).is_none());
    assert_rate(
        t.rate_at("deepseek-v4-pro", "2026-04-24", 5)
            .map(|r| r.input),
        1.74e-6,
    );
}

#[test]
fn store_walk_keeps_pricing_past_the_newest_observation() {
    // 2026-08-31 (Mon) is past the newest observed row (08-30): the windowed
    // row keeps pricing, peak windows included — observed dates never go
    // stale the way snapshot capture dates did.
    let t = store_table();
    assert_rate(
        t.rate_at("deepseek-v4-pro", "2026-08-31", 2)
            .map(|r| r.input),
        1.32e-6,
    );
}

#[test]
fn removal_row_terminates_the_key_from_its_date() {
    // MiniMax-M2.5-Lightning's 2026-08-27 row is a bare removal (no prices):
    // earlier days keep pricing its 2026-02-12 row, the removal date and
    // everything after price nothing, whatever case the ledger spells the id
    // in.
    let t = store_table();
    assert_rate(
        t.rate_at("MiniMax-M2.5-Lightning", "2026-08-26", 0)
            .map(|r| r.input),
        3e-7,
    );
    assert!(
        t.rate_at("MiniMax-M2.5-Lightning", "2026-08-27", 0)
            .is_none()
    );
    assert!(
        t.rate_at("minimax-m2.5-lightning", "2026-12-31", 12)
            .is_none()
    );
}

#[test]
fn a_removal_row_that_carries_rates_still_terminates_the_key() {
    // The removal row above is BARE, so it dashes whether or not `removed` is
    // read at all — `into_priced` finds no rates and the walk's winner has no
    // model either way. deepseek's real 2026-08-31 deepseek-r1 removal is the
    // other shape: the store keeps the last known rates ON the removal row,
    // and 15 first-party keys hold it (deepseek's retired r1/v3 line and all
    // of moonshot's v1 line, measured 2026-09-03). Only `removed` separates
    // "delisted" from "still 0.55/M" here, and both guards that read it —
    // `distill_history`'s `model: if removed { None }` and `row_for`'s early
    // return — are unbound without this row: with them deleted the key prices
    // its last rate forever instead of dashing.
    let keys = distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills");
    let r1 = keys
        .iter()
        .find(|k| k.id == "deepseek-r1")
        .expect("deepseek-r1 key");
    assert_eq!(r1.rows.len(), 2);
    let removal = &r1.rows[1];
    assert!(removal.removed, "the newest row is the removal");
    assert!(
        removal.model.is_none(),
        "a removal carries no distilled prices however many rates its line spells"
    );

    let t = store_table();
    let input = |date: &str| t.rate_at("deepseek-r1", date, 0).map(|r| r.input);
    // The day before still prices the 2025-01-20 row.
    assert_rate(input("2026-08-30"), 5.5e-7);
    // The removal date and every day after price nothing — never the
    // 0.55/M the removal row itself still spells.
    for date in ["2026-08-31", "2026-09-01", "2027-01-01"] {
        assert_eq!(input(date), None, "{date}");
    }
}

#[test]
fn reseller_history_rows_neither_price_nor_delist() {
    // together's rows for deepseek-v4-pro (markup 1.74 on 06-09, removal on
    // 08-28) and avian's (1.305 on 05-04) are reseller copies: the id prices
    // deepseek's own rows at deepseek's rates, and together's removal row
    // cannot terminate deepseek's key.
    let t = store_table();
    let input = |date: &str| t.rate_at("deepseek-v4-pro", date, 0).map(|r| r.input);
    assert_rate(input("2026-06-01"), 4.35e-7); // not avian's 1.305
    assert_rate(input("2026-06-09"), 4.35e-7); // not together's 1.74
    assert_rate(input("2026-08-29"), 6.6e-7); // past together's removal
}

#[test]
fn reseller_only_id_stays_a_dash() {
    // cloudflare is not a kept source, so the `@cf/` copy prices nothing
    // under any ladder form (the two-segment strip lands on
    // `deepseek-v4-pro-0813`, which no row carries).
    let t = store_table();
    assert!(
        t.rate_at("@cf/deepseek-ai/deepseek-v4-pro-0813", "2026-08-30", 0)
            .is_none()
    );
}

#[test]
fn store_history_dates_today_not_the_snapshot_log() {
    // Dating is the store walk for EVERY day, today included: a snapshot log
    // holding different rates never overrides it.
    let t = PriceTable::capture(
        vec![eq_model("deepseek-v4-pro", 9e-6, 9e-6)],
        distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills"),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-31".to_owned(),
        0,
        Vec::new(),
    );
    assert_rate(
        t.rate_at("deepseek-v4-pro", "2026-08-31", 0)
            .map(|r| r.input),
        6.6e-7,
    );

    // The snapshot log keeps its offline role: a table with no store history
    // still prices through the snapshot walk.
    let offline = PriceTable::capture(
        vec![eq_model("deepseek-v4-pro", 9e-6, 9e-6)],
        Vec::new(),
        Vec::new(),
        CanonicalMap::default(),
        "2026-08-31".to_owned(),
        0,
        Vec::new(),
    );
    assert_rate(
        offline
            .rate_at("deepseek-v4-pro", "2026-08-31", 0)
            .map(|r| r.input),
        9e-6,
    );
}

#[test]
fn cache_round_trips_the_store_dating_source() {
    let sandbox = HomeSandbox::new();
    let table = PriceTable {
        models: vec![eq_model("m", 1e-6, 2e-6)],
        history: vec![RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        }],
        store: distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills"),
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 5,
        memo: Mutex::default(),
    };
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_v4_price_cache.json");
    save_cache(&path, &table);

    let loaded = load_cached().expect("cache loads");
    assert_eq!(loaded.fetched_at_ms, 5);
    // Offline dating survives the round trip: a weekday peak inside the retro
    // span, and a dash before the store's oldest row.
    assert_rate(
        loaded
            .rate_at("deepseek-v4-pro", "2026-08-26", 2)
            .map(|r| r.input),
        1.32e-6,
    );
    assert!(loaded.rate_at("deepseek-v4-pro", "2026-04-23", 2).is_none());
    // With a store held, nothing prices off the snapshot log: an id only the
    // snapshot carries stays a dash.
    assert!(loaded.rate_at("m", "2026-08-19", 0).is_none());

    // An old cache (written before the store half existed) loads unchanged and
    // prices through the snapshot walk until the next fetch upgrades it.
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &path,
        r#"{"fetched_at_ms": 7, "history": [{"captured": "2026-01-01", "models": [{"id": "m", "prices": [{"input": 1e-6, "output": 2e-6, "cache_read": 0.0, "cache_write": 0.0, "constraint": null}], "effective_at": null}]}]}"#,
    )
    .expect("write");
    let old = load_cached().expect("old cache loads");
    assert_eq!(old.fetched_at_ms, 7);
    assert_eq!(
        old.rate_at("m", "2026-08-19", 0).map(|r| r.input),
        Some(1e-6)
    );
    // No store rows, so store-only dating cannot resolve.
    assert!(old.rate_at("deepseek-v4-pro", "2026-08-26", 2).is_none());
}

#[test]
fn a_price_row_after_a_removal_relists_the_key_from_its_date() {
    // The store's promised reappearance shape (its plan row 14): a fresh
    // price row appended after a removal. The index un-stamps the key then,
    // and the walk agrees — the removal wins (and dashes) the days it is
    // newest for, the fresh row re-lives the key from its own applies day.
    // Synthetic rows on the fixture's pattern; no store row exercises the
    // shape yet (verified against the 1971-row history, 2026-08-31).
    let rows = vec![
        dated_row(
            "2026-01-01",
            "2026-01-01",
            false,
            Some(eq_model("m", 1e-6, 2e-6)),
        ),
        dated_row("2026-02-01", "2026-02-01", true, None),
        dated_row(
            "2026-03-01",
            "2026-03-01",
            false,
            Some(eq_model("m", 3e-6, 6e-6)),
        ),
    ];
    let t = PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: vec![StoreKey {
            source: "deepseek".to_owned(),
            id: "m".to_owned(),
            rows,
        }],
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    let input = |date: &str| t.rate_at("m", date, 0).map(|r| r.input);
    assert_rate(input("2026-01-15"), 1e-6);
    assert_eq!(input("2026-02-15"), None); // removal is newest: dash
    assert_eq!(input("2026-02-28"), None);
    assert_rate(input("2026-03-01"), 3e-6); // reapplied: live again
}

#[test]
fn a_same_day_observed_tie_goes_to_the_later_append() {
    // zai's real glm-4.5 pair, both observed 2026-08-26 (the later append
    // added the cache-read key): the tie sits at the key's newest observed
    // day, so the pair is the walk's winner for every date from 08-26 on.
    let t = store_table();
    for date in ["2026-08-26", "2026-09-05"] {
        let rate = t.rate_at("glm-4.5", date, 0).expect("priced");
        assert!((rate.input - 6e-7).abs() < 1e-12, "{date}");
        assert!((rate.cache_read - 1.1e-7).abs() < 1e-15, "{date}");
    }
}

// ── canonical twin-ness ──────────────────────────────────────────────────────

/// A canonical-map table over the twin scenario: a live dashscope key for
/// `qwen3.6-27b` (the catalog does not name dashscope, so the bare id is its
/// own canonical), groq's slash-spelled qwen copy — the catalog maps it to
/// that same canonical — delisted beside a live deepseek-v3.2 key and
/// dashscope's same-spelled deepseek-v3.2 copy (the catalog maps both). The
/// keys are built literally rather than distilled: the resold guard drops a
/// resale before the twin-ness partition can see it, so what these rows model
/// is a CACHE written before that guard, whose keys the walk still has to
/// resolve. Synthetic rows on the fixture's pattern; the catalog entries are
/// verbatim store bytes.
fn twin_table() -> PriceTable {
    let (aliases, canonical) = (
        distill_aliases(ALIASES_FIXTURE).expect("aliases distill"),
        fixture_canonical(),
    );
    // A snapshot log holding one never-reached model: `load_cache` rejects a
    // cache with no snapshot history, and the store walk overrides the
    // snapshot for every query anyway.
    let snapshot = vec![eq_model("m", 1e-6, 2e-6)];
    PriceTable {
        models: snapshot.clone(),
        history: vec![RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: snapshot,
        }],
        store: vec![
            StoreKey {
                source: "dashscope".to_owned(),
                id: "qwen3.6-27b".to_owned(),
                rows: vec![dated_row(
                    "2026-08-29",
                    "2026-08-29",
                    false,
                    Some(eq_model("qwen3.6-27b", 0.3e-6, 0.6e-6)),
                )],
            },
            StoreKey {
                source: "groq".to_owned(),
                id: "qwen/qwen3.6-27b".to_owned(),
                rows: vec![
                    dated_row(
                        "2026-06-01",
                        "2026-06-01",
                        false,
                        Some(eq_model("qwen/qwen3.6-27b", 0.6e-6, 1.2e-6)),
                    ),
                    dated_row("2026-06-15", "2026-06-15", true, None),
                ],
            },
            StoreKey {
                source: "deepseek".to_owned(),
                id: "deepseek-v3.2".to_owned(),
                rows: vec![dated_row(
                    "2026-08-01",
                    "2026-08-01",
                    false,
                    Some(eq_model("deepseek-v3.2", 0.28e-6, 0.42e-6)),
                )],
            },
            StoreKey {
                source: "dashscope".to_owned(),
                id: "deepseek-v3.2".to_owned(),
                rows: vec![
                    dated_row(
                        "2026-06-01",
                        "2026-06-01",
                        false,
                        Some(eq_model("deepseek-v3.2", 0.57e-6, 1.71e-6)),
                    ),
                    dated_row("2026-06-15", "2026-06-15", true, None),
                ],
            },
        ],
        aliases,
        canonical,
        fetched_at_ms: 0,
        memo: Mutex::default(),
    }
}

#[test]
fn a_delisted_copy_whose_canonical_a_live_key_holds_never_prices() {
    // Cross-source twin-ness through the catalog's model registry. The raw
    // (source, id) walk prices each copy's own rows on its pre-removal days
    // (06-01..06-14: 0.6 in for the slash copy, 0.57 for the bare one) — the
    // shape this row kills. With canonical twin-ness neither copy ever
    // materializes: those days dash, and the slash spelling reprices through
    // the live first-party row's own walk via the ladder's strip.
    let t = twin_table();
    for date in ["2026-06-05", "2026-06-14", "2026-06-20"] {
        assert!(
            t.rate_at("qwen/qwen3.6-27b", date, 0).is_none(),
            "the slash copy priced {date}"
        );
        assert!(
            t.rate_at("deepseek-v3.2", date, 0).is_none(),
            "the bare copy priced {date}"
        );
    }
    assert_rate(
        t.rate_at("qwen/qwen3.6-27b", "2026-08-30", 0)
            .map(|r| r.input),
        0.3e-6,
    );
    assert_rate(
        t.rate_at("deepseek-v3.2", "2026-08-30", 0).map(|r| r.input),
        0.28e-6,
    );
}

#[test]
fn two_live_keys_on_one_canonical_keep_first_seen_order() {
    // Two live keys on one canonical id: the earlier key in store order wins
    // the ladder where both have candidates, and the later key still prices
    // the dates only it covers — canonical twin-ness never drops or reorders
    // live keys.
    let (aliases, canonical) = (
        distill_aliases(ALIASES_FIXTURE).expect("aliases distill"),
        fixture_canonical(),
    );
    let t = PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: vec![
            StoreKey {
                source: "dashscope".to_owned(),
                id: "deepseek-v3.2".to_owned(),
                rows: vec![dated_row(
                    "2026-08-29",
                    "2026-08-29",
                    false,
                    Some(eq_model("deepseek-v3.2", 0.57e-6, 1.71e-6)),
                )],
            },
            StoreKey {
                source: "deepseek".to_owned(),
                id: "deepseek-v3.2".to_owned(),
                rows: vec![dated_row(
                    "2025-12-01",
                    "2025-12-01",
                    false,
                    Some(eq_model("deepseek-v3.2", 0.28e-6, 0.42e-6)),
                )],
            },
        ],
        aliases,
        canonical,
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    // 2026-08-30: both keys have a candidate — first-seen wins the ladder.
    assert_rate(
        t.rate_at("deepseek-v3.2", "2026-08-30", 0).map(|r| r.input),
        0.57e-6,
    );
    // 2026-03-01: only the later key's row applies — it still prices.
    assert_rate(
        t.rate_at("deepseek-v3.2", "2026-03-01", 0).map(|r| r.input),
        0.28e-6,
    );
}

#[test]
fn an_unknown_pair_stays_its_own_key() {
    // (dashscope, "qwen/qwen3.6-27b") is a pair the catalog does not name —
    // its qwen3.6-27b entry lists groq's slash spelling, not dashscope's —
    // so [`PriceTable::canonical_of`]'s identity fallback keeps the copy its
    // own canonical: it still prices its own raw id on its pre-removal days
    // beside the live bare key. The two are NOT twins. A fetch can no longer
    // produce such a key (an unnamed pair is resold and drops at distill), so
    // the fallback's live reach is a cache written before the catalog half.
    let (aliases, canonical) = (
        distill_aliases(ALIASES_FIXTURE).expect("aliases distill"),
        fixture_canonical(),
    );
    let t = PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: vec![
            StoreKey {
                source: "dashscope".to_owned(),
                id: "qwen3.6-27b".to_owned(),
                rows: vec![dated_row(
                    "2026-08-29",
                    "2026-08-29",
                    false,
                    Some(eq_model("qwen3.6-27b", 0.3e-6, 0.6e-6)),
                )],
            },
            StoreKey {
                source: "dashscope".to_owned(),
                id: "qwen/qwen3.6-27b".to_owned(),
                rows: vec![
                    dated_row(
                        "2026-06-01",
                        "2026-06-01",
                        false,
                        Some(eq_model("qwen/qwen3.6-27b", 0.9e-6, 1.8e-6)),
                    ),
                    dated_row("2026-06-15", "2026-06-15", true, None),
                ],
            },
        ],
        aliases,
        canonical,
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    assert_rate(
        t.rate_at("qwen/qwen3.6-27b", "2026-06-05", 0)
            .map(|r| r.input),
        0.9e-6,
    );
    // The bare id prices the live key's own row on its dates.
    assert_rate(
        t.rate_at("qwen3.6-27b", "2026-08-30", 0).map(|r| r.input),
        0.3e-6,
    );
}

#[test]
fn canonical_map_round_trips_and_old_caches_load_identity() {
    let sandbox = HomeSandbox::new();
    let table = twin_table();
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_v4_price_cache.json");
    save_cache(&path, &table);

    let loaded = load_cached().expect("cache loads");
    // Twin-ness survives the round trip: neither copy prices 06-05.
    assert!(
        loaded
            .rate_at("qwen/qwen3.6-27b", "2026-06-05", 0)
            .is_none()
    );
    assert!(loaded.rate_at("deepseek-v3.2", "2026-06-05", 0).is_none());

    // A cache written before the canonical half existed loads empty and
    // behaves identity until the next fetch upgrades it: the slash copy — a
    // pair the map no longer resolves — prices its own rows again, while the
    // same-raw-id copy still loses to the live twin (both fall back to the
    // bare id).
    let json = std::fs::read_to_string(&path).expect("read cache");
    let mut value: serde_json::Value = serde_json::from_str(&json).expect("parse cache");
    value
        .as_object_mut()
        .expect("cache root object")
        .remove("canonical");
    std::fs::write(&path, value.to_string()).expect("write cache");
    let old = load_cached().expect("pre-canonical cache loads");
    assert_rate(
        old.rate_at("qwen/qwen3.6-27b", "2026-06-05", 0)
            .map(|r| r.input),
        0.6e-6,
    );
    assert!(old.rate_at("deepseek-v3.2", "2026-06-05", 0).is_none());
}

#[test]
fn a_pre_source_cache_loads_with_empty_sources_and_stays_identity() {
    // The serde-default upgrade path: a cache written before `StoreKey` grew
    // its `source` field loads with every key's source empty, so the
    // canonical lookup misses and twin-ness falls back to the raw id — the
    // pre-map behavior, until the next fetch persists sources. Dropping the
    // `#[serde(default)]` on the field makes this cache fail to load and
    // reds the pin.
    let sandbox = HomeSandbox::new();
    let table = twin_table();
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_v4_price_cache.json");
    save_cache(&path, &table);

    let json = std::fs::read_to_string(&path).expect("read cache");
    let mut value: serde_json::Value = serde_json::from_str(&json).expect("parse cache");
    for key in value["store"].as_array_mut().expect("store array") {
        key.as_object_mut()
            .expect("store key object")
            .remove("source");
    }
    std::fs::write(&path, value.to_string()).expect("write cache");
    let old = load_cached().expect("pre-source cache loads");
    // Empty sources: the same-raw-id copy still loses to the live twin (both
    // canonicalize to the bare id), and the slash copy prices again (its raw
    // id differs from the live key's).
    assert!(old.rate_at("deepseek-v3.2", "2026-06-05", 0).is_none());
    assert_rate(
        old.rate_at("qwen/qwen3.6-27b", "2026-06-05", 0)
            .map(|r| r.input),
        0.6e-6,
    );
}

// ── api aliases and variant spellings ────────────────────────────────────────

/// Trimmed real ai-pricelog v4 model registry
/// (`tests/fixtures/ai-pricelog-models-trimmed.json`): every canonical the
/// index and history fixtures name, each carrying its `vendor` — the maker
/// half of the resold comparison — plus qwen3.6-27b for the canonical-twin
/// pins' cross-source spelling pair (groq's slash id beside the bare one).
/// Every real entry is verbatim store bytes; the three `synth-*` ids the
/// index fixture invents carry a matching entry so a synthetic row is
/// first-party for the reason a real one is. Query-side ids are test
/// literals, never fixture rows.
const MODELS_FIXTURE: &str = include_str!("../fixtures/ai-pricelog-models-trimmed.json");

/// A store-walk table that also carries the fixture's alias chains and
/// canonical map.
fn aliased_table() -> PriceTable {
    let (aliases, canonical) = (
        distill_aliases(ALIASES_FIXTURE).expect("aliases distill"),
        fixture_canonical(),
    );
    PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills"),
        aliases,
        canonical,
        fetched_at_ms: 0,
        memo: Mutex::default(),
    }
}

#[test]
fn aliases_fixture_distills_three_chains() {
    let keys = distill_aliases(ALIASES_FIXTURE).expect("aliases distill");
    let ids: Vec<&str> = keys.iter().map(|k| k.id.as_str()).collect();
    assert_eq!(
        ids,
        ["deepseek-chat", "deepseek-reasoner", "grok-4.5-latest"]
    );
    let chat = &keys[0];
    assert_eq!(chat.spans.len(), 7);
    assert_eq!(chat.spans[0].from.as_deref(), Some("2024-12-26"));
    assert_eq!(chat.spans[0].to.as_deref(), Some("2025-03-24"));
    assert_eq!(chat.spans[0].canonical, "deepseek-v3");
    // The last record closes the chain: deepseek retired the alias 2026-07-24.
    assert_eq!(chat.spans[6].to.as_deref(), Some("2026-07-24"));
    assert_eq!(chat.spans[6].canonical, "deepseek-v4-flash");
    // The null-bound shape: a chain that has always pointed at one live
    // canonical, both ends open.
    let grok = &keys[2];
    assert_eq!(grok.spans.len(), 1);
    assert_eq!(grok.spans[0].from, None);
    assert_eq!(grok.spans[0].to, None);
    assert_eq!(grok.spans[0].canonical, "grok-4.5");

    // Tolerance: a record without a canonical skips, an empty chain drops, a
    // non-array chain drops; zero surviving aliases fails the feed.
    let mixed = r#"{"version": 4, "aliases": {
        "a": [{"from": "2026-01-01", "canonical": "m"}],
        "b": [{"from": "2026-01-01"}],
        "c": [],
        "d": 7
    }}"#;
    let keys = distill_aliases(mixed).expect("one chain survives");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].id, "a");
    assert_eq!(keys[0].spans[0].canonical, "m");
    assert!(distill_aliases("{}").is_err());
    assert!(distill_aliases(r#"{"aliases": {}}"#).is_err());
    assert!(distill_aliases("not json").is_err());
    assert!(distill_aliases(r#"{"models": {}}"#).is_err());
}

#[test]
fn the_catalog_keys_a_pair_on_the_source_the_feeds_spell() {
    // The four feed files spell all 29 sources identically (measured
    // 2026-09-03), so the canonical map's key and a `StoreKey`'s source come
    // from one vocabulary and no rename sits between them: `xai` on both
    // sides, never a clauth-side alias for it.
    let canonical = fixture_canonical();
    assert_eq!(
        canonical.get("xai").and_then(|m| m.get("grok-4.5")),
        Some(&"grok-4.5".to_owned())
    );
    assert_eq!(
        canonical.get("moonshot").and_then(|m| m.get("kimi-k2.6")),
        Some(&"kimi-k2.6".to_owned())
    );
    // Same vocabulary on the store side.
    let keys = distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills");
    let grok = keys.iter().find(|k| k.id == "grok-4.5").expect("grok key");
    assert_eq!(grok.source, "xai");
    assert_eq!(
        canonical
            .get(&grok.source)
            .and_then(|m| m.get(&grok.id))
            .map(String::as_str),
        Some("grok-4.5"),
        "the store key resolves against the map with no rename"
    );
    // Tolerance: a missing `models` object and an empty one both fail.
    let vendors = distill_providers(PROVIDERS_FIXTURE).expect("providers distill");
    assert!(distill_catalog("{}", &vendors).is_err());
    assert!(distill_catalog(r#"{"models": {}}"#, &vendors).is_err());
    assert!(distill_catalog("not json", &vendors).is_err());
    // An entry with no `sources` object skips instead of failing the feed.
    let mixed = r#"{"version": 4, "models": {
        "a": {"vendor": "deepseek", "sources": {"deepseek": ["a"]}},
        "b": {"vendor": "deepseek"},
        "c": {"vendor": "deepseek", "sources": 7}
    }}"#;
    let (canonical, _) = distill_catalog(mixed, &vendors).expect("one entry survives");
    assert_eq!(canonical.len(), 1);
    assert_eq!(
        canonical.get("deepseek").and_then(|m| m.get("a")),
        Some(&"a".to_owned())
    );
}

#[test]
fn api_alias_prices_the_canonicals_dated_rate() {
    // deepseek-chat's first record covers 2024-12-26 .. 2025-03-24 and names
    // deepseek-v3: a day inside it prices deepseek-v3's own store walk, at
    // v3's row rates.
    let t = aliased_table();
    let rate = t.rate_at("deepseek-chat", "2025-02-10", 0).expect("priced");
    assert_rate(Some(rate.input), 2.7e-7);
    assert_rate(Some(rate.cache_read), 7e-8);
    assert_rate(Some(rate.output), 1.1e-6);
    // The alias resolves but the canonical's walk has no row that early: the
    // store's oldest deepseek-v3 observation is 2025-02-08, and the days
    // before it dash like any store-less day.
    assert!(t.rate_at("deepseek-chat", "2025-01-15", 0).is_none());
}

#[test]
fn api_alias_walks_its_chain_by_day() {
    let t = aliased_table();
    // deepseek-reasoner's chain: R1 from 2025-01-20, v3.2 from 2025-12-01,
    // v4-flash from 2026-04-24 — three priced canonicals across the chain.
    let r1 = t
        .rate_at("deepseek-reasoner", "2025-02-01", 0)
        .expect("r1 priced");
    assert_rate(Some(r1.input), 5.5e-7);
    assert_rate(Some(r1.cache_read), 1.4e-7);
    assert_rate(Some(r1.output), 2.19e-6);
    // A record's `from` is inclusive and the previous record's `to`
    // exclusive: 2026-04-23 prices v3.2, the day after prices v4-flash.
    assert_rate(
        t.rate_at("deepseek-reasoner", "2026-04-23", 0)
            .map(|r| r.input),
        2.8e-7,
    );
    assert_rate(
        t.rate_at("deepseek-reasoner", "2026-04-24", 0)
            .map(|r| r.input),
        1.4e-7,
    );
}

#[test]
fn a_null_bound_alias_prices_for_every_day() {
    // grok-4.5-latest's single record has both bounds open: it prices at
    // grok-4.5's own walk on any day the store covers, old and new alike.
    let t = aliased_table();
    for date in ["2026-07-09", "2026-08-30"] {
        assert_rate(t.rate_at("grok-4.5-latest", date, 0).map(|r| r.input), 2e-6);
    }
}

#[test]
fn api_alias_retired_past_its_last_record_dashes() {
    // deepseek retired both api aliases on 2026-07-24: the retirement day
    // itself (a `to`, exclusive) and everything after price nothing, even
    // though the last canonical (deepseek-v4-flash) is still live.
    let t = aliased_table();
    for id in ["deepseek-chat", "deepseek-reasoner"] {
        assert_rate(t.rate_at(id, "2026-07-23", 0).map(|r| r.input), 1.4e-7);
        for date in ["2026-07-24", "2026-08-30", "2027-01-01"] {
            assert!(t.rate_at(id, date, 0).is_none(), "{id} priced at {date}");
        }
    }
}

#[test]
fn an_alias_only_resolves_once_the_ladder_misses() {
    // A row carrying the alias id verbatim wins — the chain never remaps an
    // id the table prices (no live row carries `deepseek-chat`; this is the
    // guard for the store growing one whose rates differ from the chain's
    // canonical).
    let (aliases, canonical) = (
        distill_aliases(ALIASES_FIXTURE).expect("aliases distill"),
        fixture_canonical(),
    );
    let t = PriceTable {
        models: vec![eq_model("deepseek-chat", 9e-6, 9e-6)],
        history: vec![RateSnapshot {
            captured: "2024-01-01".to_owned(),
            models: vec![eq_model("deepseek-chat", 9e-6, 9e-6)],
        }],
        store: Vec::new(),
        aliases,
        canonical,
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    assert_rate(
        t.rate_at("deepseek-chat", "2025-02-10", 0).map(|r| r.input),
        9e-6,
    );
}

#[test]
fn variant_suffix_prices_the_base_id() {
    // A claude session on a deepseek api profile spells the served model with
    // a `-thinking` suffix no page id carries. The strip runs only after the
    // ladder misses and retries the base id through the full walk,
    // case-insensitively like the ladder; the bracket strip composes with it.
    let t = store_table();
    for id in [
        "deepseek-v4-pro-thinking",
        "DeepSeek-V4-Pro-Thinking",
        "deepseek-v4-pro-thinking[1m]",
    ] {
        assert_rate(t.rate_at(id, "2026-08-16", 0).map(|r| r.input), 6.6e-7);
    }
}

#[test]
fn a_verbatim_id_is_never_remapped_by_the_variant_strip() {
    // No kept first-party id ends in `-thinking` today (the store's thinking
    // ids are all reseller spellings), so the shape is a literal table: a row
    // that carries the suffix itself prices its own rates, never the base id
    // under it.
    let t = table(vec![
        eq_model("m-thinking", 3e-6, 6e-6),
        eq_model("m", 1e-6, 2e-6),
    ]);
    assert_rate(
        t.rate_at("m-thinking", "2026-08-19", 0).map(|r| r.input),
        3e-6,
    );
}

#[test]
fn history_rows_with_unmodeled_keys_parse_and_a_resold_one_stays_dropped() {
    // openrouter's real row carries a `fees` object and two rate axes clauth
    // has no bucket for. A FIRST-PARTY row carrying the same keys must PARSE
    // — the line's survival is the parser's tolerance, not the resold guard's
    // drop — and price its four modeled axes.
    let kept = concat!(
        r#"{"schema":4,"source":"deepseek","model_id":"m","observed_at":"2026-01-01","#,
        r#""rates":{"input":1,"output":2,"cache_write_1h":0.5,"internal_reasoning":9},"#,
        r#""fees":{"web_search":0.01},"limits":{"context":200000},"#,
        r#""provenance":{"name":"probe"}}"#,
        "\n"
    );
    let keys = distill_history(kept, &all_first_party_history(kept))
        .expect("unmodeled keys do not fail the line");
    assert_eq!(keys.len(), 1);
    let t = PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: keys,
        aliases: Vec::new(),
        canonical: CanonicalMap::default(),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    };
    assert_rate(t.rate_at("m", "2026-01-01", 0).map(|r| r.input), 1e-6);

    // The fixture's verbatim openrouter row never enters the walk: the id has
    // no key and prices nothing. Distilling the same bytes against a guard
    // that keeps every pair is the control — the row parses, so what dropped
    // it is the vendor comparison and not a parse failure.
    let keys = distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills");
    assert!(!keys.iter().any(|k| k.id == "anthropic/claude-3-haiku"));
    let unguarded = distill_history(HISTORY_FIXTURE, &all_first_party_history(HISTORY_FIXTURE))
        .expect("history distills");
    assert!(
        unguarded.iter().any(|k| k.id == "anthropic/claude-3-haiku"),
        "the line parses; only the resold guard drops it"
    );
    let t = store_table();
    assert!(
        t.rate_at("anthropic/claude-3-haiku", "2026-08-26", 0)
            .is_none()
    );
}

#[test]
fn cache_round_trips_the_alias_table_and_old_caches_load_without_it() {
    let sandbox = HomeSandbox::new();
    let (aliases, canonical) = (
        distill_aliases(ALIASES_FIXTURE).expect("aliases distill"),
        fixture_canonical(),
    );
    let table = PriceTable {
        models: vec![eq_model("m", 1e-6, 2e-6)],
        history: vec![RateSnapshot {
            captured: "2026-01-01".to_owned(),
            models: vec![eq_model("m", 1e-6, 2e-6)],
        }],
        store: distill_history(HISTORY_FIXTURE, &fixture_guard()).expect("history distills"),
        aliases,
        canonical,
        fetched_at_ms: 11,
        memo: Mutex::default(),
    };
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_v4_price_cache.json");
    save_cache(&path, &table);

    let loaded = load_cached().expect("cache loads");
    assert_eq!(loaded.fetched_at_ms, 11);
    assert_rate(
        loaded
            .rate_at("deepseek-chat", "2025-02-10", 0)
            .map(|r| r.input),
        2.7e-7,
    );
    assert!(loaded.rate_at("deepseek-chat", "2026-08-30", 0).is_none());

    // A cache written before the aliases half existed keeps dating through
    // its store half but no alias resolves until the next fetch upgrades it.
    let json = std::fs::read_to_string(&path).expect("read cache");
    let mut value: serde_json::Value = serde_json::from_str(&json).expect("parse cache");
    value
        .as_object_mut()
        .expect("cache root object")
        .remove("aliases");
    std::fs::write(&path, value.to_string()).expect("write cache");
    let old = load_cached().expect("pre-aliases cache loads");
    assert!(old.rate_at("deepseek-chat", "2025-02-10", 0).is_none());
    assert_rate(
        old.rate_at("deepseek-v3", "2025-02-10", 0).map(|r| r.input),
        2.7e-7,
    );
}

/// A literal history row for the synthetic-shape tests.
fn dated_row(observed: &str, applies: &str, removed: bool, model: Option<PricedModel>) -> StoreRow {
    StoreRow {
        observed: observed.to_owned(),
        applies: applies.to_owned(),
        removed,
        model,
    }
}
