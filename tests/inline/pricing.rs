//! Inline tests for `crate::pricing` — ai-pricelog distill, id resolution,
//! constraint selection, snapshot history, and per-model cost math. No
//! network: tables are built from literals and the trimmed real-index
//! fixture.

use super::*;

use crate::logline::LogLines;
use crate::testutil::HomeSandbox;

/// Trimmed real ai-pricelog index
/// (`tests/fixtures/ai-pricelog-index-trimmed.json`): real rows for the
/// claude / gpt / qwen / glm / deepseek / moonshot / grok first-party
/// representatives, dashscope's six `removed_at`-stamped resold rows, one
/// reseller source, and rows marked `"synthetic": true` for shapes no live
/// row exercises (the legacy peak_windows shape, a future `effective_at`, an
/// overlapping window pair).
const FIXTURE: &str = include_str!("../fixtures/ai-pricelog-index-trimmed.json");

// ── helpers ──────────────────────────────────────────────────────────────────

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
    let json = r#"{"version": 3, "sources": {"deepseek": {
        "deepseek-v4-pro": {"input_mtok": 0.66, "output_mtok": 1.98, "cache_read_mtok": 0.022}
    }}}"#;
    let models = distill(json).expect("distill ok");
    assert_eq!(models.len(), 1);
    let rate = &models[0].prices[0];
    assert!((rate.input - 6.6e-7).abs() < 1e-12, "got {}", rate.input);
    assert!((rate.output - 1.98e-6).abs() < 1e-12, "got {}", rate.output);
    assert!((rate.cache_read - 2.2e-8).abs() < 1e-15);
    assert_eq!(rate.cache_write, 0.0); // missing field defaults to 0
}

#[test]
fn distill_skips_rows_with_malformed_price_fields() {
    // The index carries no tiered ladders (verified against the live index);
    // a declared rate key of any non-numeric shape fails the whole row, which
    // the caller skips — the sibling row survives.
    let json = r#"{"version": 3, "sources": {"deepseek": {
        "bad-price-shape": {"input_mtok": "garbage", "output_mtok": 0.42},
        "deepseek-v3.2": {"input_mtok": 0.28, "output_mtok": 0.42}
    }}}"#;
    let models = distill(json).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["deepseek-v3.2"]);
}

#[test]
fn distill_keeps_first_party_drops_resellers() {
    let json = r#"{"version": 3, "sources": {
        "deepseek": {
            "deepseek-v3.2": {"input_mtok": 0.28, "output_mtok": 0.42}
        },
        "openrouter": {
            "deepseek/deepseek-v3.2": {"input_mtok": 3, "output_mtok": 15}
        },
        "novita": {
            "deepseek/deepseek-v3.2": {"input_mtok": 5, "output_mtok": 25}
        },
        "avian": {
            "deepseek/deepseek-v3.2": {"input_mtok": 1, "output_mtok": 2}
        }
    }}"#;
    let models = distill(json).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["deepseek-v3.2"]);
}

#[test]
fn distill_maps_source_names_and_drops_closed_sources() {
    // The index spells the two renamed providers differently from clauth's
    // canonical names; the map applies at distill. zhipuai and voyageai are
    // closed upstream and stay out of the allowlist.
    let json = r#"{"version": 3, "sources": {
        "moonshot": {"kimi-k2.6": {"input_mtok": 0.95, "output_mtok": 4.0}},
        "xai": {"grok-4.6": {"input_mtok": 2.0, "output_mtok": 6.0}},
        "zhipuai": {"glm-4.7": {"input_mtok": 0.5, "output_mtok": 1.0}},
        "voyageai": {"voyage-3": {"input_mtok": 0.1, "output_mtok": 0.1}}
    }}"#;
    let models = distill(json).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["kimi-k2.6", "grok-4.6"]);
}

#[test]
fn distill_drops_resold_claude_rows() {
    // A kept provider reselling another vendor's models: its claude rows drop
    // and anthropic's own row is the only legitimate one. The live index
    // carries no such rows today; the guard stays against upstream regress.
    let json = r#"{"version": 3, "sources": {
        "anthropic": {
            "claude-opus-5": {"input_mtok": 5, "output_mtok": 25}
        },
        "google": {
            "claude-opus-5": {"input_mtok": 3, "output_mtok": 15},
            "gemini-3.7-flash": {"input_mtok": 0.1, "output_mtok": 0.4}
        }
    }}"#;
    let models = distill(json).expect("distill ok");
    let count = |id: &str| models.iter().filter(|m| m.id == id).count();
    assert_eq!(count("claude-opus-5"), 1); // anthropic's row survives
    assert_eq!(count("gemini-3.7-flash"), 1);
}

#[test]
fn distill_drops_capitalized_resold_claude_rows() {
    // The prefix check is case-insensitive; a capitalized resold id must
    // drop exactly like its lowercase twin.
    let json = r#"{"version": 3, "sources": {
        "google": {
            "Claude-3-5-Sonnet": {"input_mtok": 3, "output_mtok": 15},
            "gemini-3.7-flash": {"input_mtok": 0.1, "output_mtok": 0.4}
        }
    }}"#;
    let models = distill(json).expect("distill ok");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gemini-3.7-flash"]);
}

#[test]
fn distill_parses_window_rates_and_effective_at() {
    // deepseek-v4-pro's real shape: flat base rates, a row-level effective
    // date, and two weekday window entries with HHMM windows.
    let json = r#"{"version": 3, "sources": {"deepseek": {
        "deepseek-v4-pro": {
            "input_mtok": 0.66, "output_mtok": 1.98, "cache_read_mtok": 0.022,
            "effective_at": "2026-08-23",
            "window_rates": [
                {"window": [100, 400],
                 "days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
                 "input_mtok": 1.32, "output_mtok": 3.96, "cache_read_mtok": 0.044},
                {"window": [600, 1000],
                 "days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
                 "input_mtok": 1.32, "output_mtok": 3.96, "cache_read_mtok": 0.044}
            ]
        }
    }}}"#;
    let models = distill(json).expect("distill ok");
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
fn distill_window_entry_inherits_missing_keys_from_base() {
    // A window entry carries override rates only; the keys it leaves absent
    // price at the row's base.
    let json = r#"{"version": 3, "sources": {"zai": {
        "m": {
            "input_mtok": 0.1, "output_mtok": 0.2, "cache_read_mtok": 0.01,
            "window_rates": [
                {"window": [100, 400], "input_mtok": 0.3}
            ]
        }
    }}}"#;
    let models = distill(json).expect("distill ok");
    let entries = &models[0].prices;
    assert_eq!(entries.len(), 2);
    assert!((entries[1].input - 3e-7).abs() < 1e-12);
    assert!(
        (entries[1].output - 2e-7).abs() < 1e-12,
        "output inherits base"
    );
    assert!(
        (entries[1].cache_read - 1e-8).abs() < 1e-15,
        "cache_read inherits base"
    );
    assert_eq!(entries[1].cache_write, 0.0, "absent on row and entry");
}

#[test]
fn distill_skips_quota_only_entries() {
    // `quota_multiplier` is a consumption weight, never a rate: an entry with
    // no rate keys of its own is skipped at distill (the zai glm rows carry
    // these), while a quota key ON a rated entry is ignored and the rates
    // still contribute.
    let json = r#"{"version": 3, "sources": {"zai": {
        "glm-5.3-flash": {
            "input_mtok": 0.075, "output_mtok": 0.25, "cache_read_mtok": 0.015,
            "window_rates": [
                {"quota_multiplier": 0.4},
                {"quota_multiplier": 1.2,
                 "days": ["monday", "tuesday", "wednesday", "thursday", "friday"],
                 "window": [600, 1000]}
            ]
        },
        "quota-plus-rates": {
            "input_mtok": 0.1, "output_mtok": 0.2,
            "window_rates": [
                {"quota_multiplier": 1.5, "window": [100, 400], "input_mtok": 0.5}
            ]
        }
    }}}"#;
    let models = distill(json).expect("distill ok");
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
fn distill_skips_malformed_entries_not_the_row() {
    // A window entry whose window violates the generator's bounds (hours
    // > 24) skips only itself; the base entry and the good sibling keep the
    // row priced.
    let json = r#"{"version": 3, "sources": {"deepseek": {
        "probe": {
            "input_mtok": 0.1, "output_mtok": 0.2,
            "window_rates": [
                {"window": [9000, 9500], "input_mtok": 0.3},
                {"window": [100, 400], "input_mtok": 0.4}
            ]
        }
    }}}"#;
    let models = distill(json).expect("good entries survive");
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
    // Only resellers → zero models → the fetch fails rather than shipping an
    // empty table.
    let json = r#"{"version": 3, "sources": {
        "openrouter": {"z-ai/glm-4.7": {"input_mtok": 0.5, "output_mtok": 1}}
    }}"#;
    assert!(distill(json).is_err());
    // The genai-prices shape (an array root) is not this feed's shape.
    assert!(distill(r#"[{"id": "deepseek", "models": []}]"#).is_err());
    assert!(distill("{}").is_err());
    assert!(distill("not json").is_err());
}

#[test]
fn distill_skips_unparseable_rows_and_sources() {
    let json = r#"{"version": 3, "sources": {
        "deepseek": {
            "good": {"input_mtok": 0.28, "output_mtok": 0.42},
            "bad-price-shape": {"input_mtok": "garbage"},
            "no-token-price": {"web_search_usd": 10}
        },
        "not-an-object-source": 42,
        "zhipuai": {
            "no-token-price": {"input_mtok": 1, "output_mtok": 1}
        }
    }}"#;
    let models = distill(json).expect("one good model survives");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["good"]);
}

#[test]
fn unknown_index_version_warns_and_still_parses() {
    let lines = LogLines::new();
    let _guard = lines.capture_here();
    let json = r#"{"version": 99, "sources": {
        "openai": {"gpt-4.1": {"input_mtok": 2.0, "output_mtok": 8.0}}
    }}"#;
    let models = distill(json).expect("parses best-effort");
    assert_eq!(models.len(), 1);
    let missing = r#"{"sources": {
        "openai": {"gpt-4.1": {"input_mtok": 2.0, "output_mtok": 8.0}}
    }}"#;
    assert_eq!(distill(missing).expect("parses best-effort").len(), 1);
    let got = lines.snapshot();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(
        got[0].contains("version 99") && got[0].contains("parsing best-effort"),
        "{got:?}"
    );
    assert!(got[1].contains("version missing"), "{got:?}");
}

#[test]
fn legacy_peak_windows_map_to_window_entries() {
    // The legacy shape: `peak_windows` as ["HH:MM","HH:MM"] STRING pairs plus
    // flat `peak_*` rate keys. No live row carries it (verified against the
    // live index); the fixture row is synthetic.
    let json = r#"{"version": 3, "sources": {"deepseek": {
        "legacy": {
            "input_mtok": 0.1, "output_mtok": 0.2, "cache_read_mtok": 0.01,
            "peak_windows": [["01:00", "04:00"]],
            "peak_input_mtok": 0.2, "peak_output_mtok": 0.4
        }
    }}}"#;
    let models = distill(json).expect("distill ok");
    let t = table(models);
    let at = |hour: u8| t.rate_at("legacy", "2026-08-29", hour).expect("priced");
    assert!((at(2).input - 2e-7).abs() < 1e-12);
    assert!((at(2).output - 4e-7).abs() < 1e-12);
    assert!(
        (at(2).cache_read - 1e-8).abs() < 1e-15,
        "absent peak key inherits base"
    );
    assert!(
        (at(5).input - 1e-7).abs() < 1e-12,
        "outside the peak window"
    );
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
        distill(FIXTURE).expect("fixture distills"),
        Vec::new(),
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
        distill(FIXTURE).expect("fixture distills"),
        Vec::new(),
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
        distill(FIXTURE).expect("fixture distills"),
        Vec::new(),
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
        distill(FIXTURE).expect("fixture distills"),
        Vec::new(),
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
        "2026-08-19".to_owned(),
        42,
        Vec::new(),
    );
    assert_eq!(t.history.len(), 1);

    // Identical refetch: same models → no new snapshot, capture date dropped.
    let t2 = PriceTable::capture(
        vec![eq_model("m", 1e-6, 2e-6)],
        Vec::new(),
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
        "2026-01-01".to_owned(),
        0,
        Vec::new(),
    );
    for i in 1..=182u32 {
        let rate = f64::from(i) * 1e-6;
        t = PriceTable::capture(
            vec![eq_model("m", rate, 0.0)],
            Vec::new(),
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
        fetched_at_ms: 12345,
        memo: Mutex::default(),
    };
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_price_cache.json");
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
        .join("ai_pricelog_price_cache.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, r#"{"fetched_at_ms": 1, "history": []}"#).expect("write");
    assert!(load_cached().is_none());
}

#[test]
fn stale_caches_deleted_once() {
    // Both pre-ai-pricelog cache files go in one cleanup pass after the first
    // successful new-cache write; the flag is set before the deletes, so a
    // reappearing file is never re-deleted.
    let sandbox = HomeSandbox::new();
    let new_path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_price_cache.json");
    let lite = sandbox.home().join(".clauth").join("price_cache.json");
    let genai = sandbox
        .home()
        .join(".clauth")
        .join("genai_price_cache.json");
    std::fs::create_dir_all(lite.parent().expect("parent")).expect("mkdir");
    std::fs::write(&lite, "{}").expect("write");
    std::fs::write(&genai, "{}").expect("write");

    let mut done = false;
    delete_stale_cache_once(&new_path, &mut done);
    assert!(done);
    assert!(!lite.exists());
    assert!(!genai.exists());

    std::fs::write(&lite, "{}").expect("write");
    std::fs::write(&genai, "{}").expect("write");
    delete_stale_cache_once(&new_path, &mut done);
    assert!(lite.exists());
    assert!(genai.exists());
}

// ── real-index fixture ───────────────────────────────────────────────────────

#[test]
fn fixture_distills_resolvers_and_excludes_resellers() {
    let models = distill(FIXTURE).expect("fixture distills");
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    // The reseller source is dropped entirely.
    assert!(!ids.contains(&"aion-labs/aion-2.0"));
    // The kept first-party representatives survive, the mapped spellings
    // included.
    for id in [
        "claude-opus-5",
        "gpt-4.1",
        "qwen3.8-max",
        "glm-5.3-flash",
        "kimi-k2.6",
        "grok-4.6",
        "deepseek-v4-pro",
        "synth-future-effective",
        "synth-legacy-peak-windows",
        "synth-overlap",
    ] {
        assert!(ids.contains(&id), "{id} missing");
    }
    // The six dashscope resold rows carry `removed_at` in the fixture (the
    // post-fix index shape) and delist at distill.
    for id in [
        "deepseek-v3.2",
        "deepseek-v4-flash",
        "glm-5.1",
        "glm-5.2",
        "kimi-k2.7-code",
    ] {
        assert!(!ids.contains(&id), "removed row {id} must delist");
    }

    let t = PriceTable::capture(models, Vec::new(), "2026-08-30".to_owned(), 0, Vec::new());
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

    // The synthetic legacy row prices through its peak window.
    assert_rate(
        t.rate_at("synth-legacy-peak-windows", "2026-08-29", 2)
            .map(|r| r.input),
        2e-7,
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
    let models = distill(FIXTURE).expect("fixture distills");
    let flash = models
        .iter()
        .find(|m| m.id == "glm-5.3-flash")
        .expect("row distills");
    assert_eq!(flash.prices.len(), 1, "quota entries contribute no rates");
    let t = PriceTable::capture(models, Vec::new(), "2026-08-30".to_owned(), 0, Vec::new());
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
    // The fixture's dashscope resold rows are real post-fix rows: the index
    // keeps them with their last prices and a `removed_at` stamp. A delisted
    // row prices nothing at any date or hour — the ids with no live
    // first-party twin stay unpriced.
    let t = PriceTable::capture(
        distill(FIXTURE).expect("fixture distills"),
        Vec::new(),
        "2026-08-30".to_owned(),
        0,
        Vec::new(),
    );
    for id in [
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

/// Trimmed real ai-pricelog history
/// (`tests/fixtures/ai-pricelog-history-trimmed.ndjson`): five of deepseek's
/// seven deepseek-v4-pro rows (the 2026-08-30 retro-effective windowed row,
/// the 2026-08-26 legacy `peak_windows` row, the 08-16 / 05-22 / 04-24 flat
/// rows), minimax's MiniMax-M2.5-Lightning
/// price + bare-removal pair, two of together's four deepseek-v4-pro rows (the
/// 06-09 markup and the 08-28 removal), one of avian's two (the 05-04
/// markup), cloudflare's one row of its forty
/// (`@cf/deepseek-ai/deepseek-v4-pro-0813`, the dropped source's id behind the
/// reseller-dash pin), dashscope's resold deepseek-v3.2 price +
/// removal pair beside deepseek's own row (the delisted-copy shadow case:
/// dashscope's key is first-seen earlier), and zai's glm-4.5 same-day pair
/// (a kept-key tie group that differs in rates at its key's newest observed
/// day). Every line is verbatim store bytes, in store order.
const HISTORY_FIXTURE: &str = include_str!("../fixtures/ai-pricelog-history-trimmed.ndjson");

/// A table whose only dating source is the history fixture — no snapshot log,
/// so every date resolves through the store walk.
fn store_table() -> PriceTable {
    PriceTable {
        models: Vec::new(),
        history: Vec::new(),
        store: distill_history(HISTORY_FIXTURE).expect("history distills"),
        fetched_at_ms: 0,
        memo: Mutex::default(),
    }
}

#[test]
fn history_distills_first_party_keys_only() {
    let keys = distill_history(HISTORY_FIXTURE).expect("history distills");
    let ids: Vec<&str> = keys.iter().map(|k| k.id.as_str()).collect();
    // First-seen store order (dashscope's resold deepseek-v3.2 key before
    // deepseek's own); together / avian / cloudflare rows never enter, so a
    // non-kept source's row can neither price an id nor delist it.
    assert_eq!(
        ids,
        [
            "MiniMax-M2.5-Lightning",
            "deepseek-v4-pro",
            "glm-4.5",
            "deepseek-v3.2",
            "deepseek-v3.2"
        ]
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
    let mixed = "{\"source\":\"deepseek\",\"model_id\":\"m\",\"observed_at\":\"2026-01-01\",\"input_mtok\":1,\"output_mtok\":2}\nnot json\n{}\n";
    assert_eq!(distill_history(mixed).expect("one key").len(), 1);
    assert!(distill_history("").is_err());
    assert!(distill_history("not json at all").is_err());
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
        distill_history(HISTORY_FIXTURE).expect("history distills"),
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
        store: distill_history(HISTORY_FIXTURE).expect("history distills"),
        fetched_at_ms: 5,
        memo: Mutex::default(),
    };
    let path = sandbox
        .home()
        .join(".clauth")
        .join("ai_pricelog_price_cache.json");
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
fn a_delisted_keys_copy_never_shadows_the_live_first_party_row() {
    // dashscope (kept: it owns qwen) resells other vendors' ids; the store
    // delisted the copies on 2026-08-31, and dashscope's deepseek-v3.2 key is
    // FIRST-SEEN before deepseek's own. The delisting is the evidence the key
    // was reselling: live keys materialize first, so the delisted copy's
    // pre-removal days (08-29, 08-30) price deepseek's own row — 0.28 in,
    // 0.028 cache-read — never dashscope's 0.57 markup — and dashscope's
    // removal cannot touch deepseek's key.
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
            id: "m".to_owned(),
            rows,
        }],
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

/// A literal history row for the synthetic-shape tests.
fn dated_row(observed: &str, applies: &str, removed: bool, model: Option<PricedModel>) -> StoreRow {
    StoreRow {
        observed: observed.to_owned(),
        applies: applies.to_owned(),
        removed,
        model,
    }
}
