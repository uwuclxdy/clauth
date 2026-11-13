use super::*;

use crate::pricing::HourTokens;
use crate::tokens::{DayModelTokens, ModelTokens, TokenStats, UsageShape};

// ── helpers ──────────────────────────────────────────────────────────────────

fn split(model: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: model.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
        ..Default::default()
    }
}

/// A merged base carrying transcript-derived (split-bearing) per-day rows — the
/// shape `merge_topup` hands to [`Ledger::record_covering`]. The rows carry no hourly
/// buckets (the v1-ledger path).
fn base_with(days: &[(&str, ModelTokens)]) -> TokenStats {
    let mut b = TokenStats::default();
    for (date, m) in days {
        b.daily_models.push(DayModelTokens {
            date: (*date).to_owned(),
            model: m.model.clone(),
            in_out: m.in_out(),
            split: Some(m.clone()),
            hours: None,
        });
    }
    b
}

#[test]
fn loading_ledger_discards_placeholder_models_and_keeps_real_splits() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("create ledger dir");
    std::fs::write(
        dir.join("token_ledger.json"),
        r#"{"recorded_through":"2026-06-16","days":{"2026-06-16":{"<synthetic>":{"input":0,"output":0,"cache_read":0,"cache_create":0},"model<placeholder>":{"input":3,"output":2,"cache_read":0,"cache_create":0},"minimax/minimax-m3:free":{"input":8,"output":2,"cache_read":4,"cache_create":1,"shape":"no_cache_writes"}}}}"#,
    )
    .expect("write ledger");
    let ledger = Ledger::load(&dir);
    let mut base = TokenStats::default();
    ledger.apply_to_base(&mut base, Some("2026-06-01"));
    assert_eq!(base.models.len(), 1);
    assert_eq!(base.models[0].model, "minimax/minimax-m3:free");
    assert_eq!(
        (
            base.models[0].input,
            base.models[0].output,
            base.models[0].cache_read,
            base.models[0].cache_create
        ),
        (8, 2, 4, 1)
    );
    assert_eq!(base.daily_models.len(), 1);
    assert_eq!(base.daily_models[0].model, "minimax/minimax-m3:free");
    assert_eq!(base.daily[0].tokens, 10);
    assert_eq!(
        base.daily_models[0].split.as_ref().expect("split").shape,
        crate::tokens::UsageShape::NoCacheWrites
    );
    assert_eq!(
        serde_json::to_value(&ledger).expect("serialize ledger")["days"]["2026-06-16"],
        serde_json::json!({"minimax/minimax-m3:free": {"input": 8, "output": 2, "cache_read": 4, "cache_create": 1, "shape": "no_cache_writes"}})
    );
}

// ── durability ────────────────────────────────────────────────────────────────

/// The core guarantee: a day recorded from transcripts is fully reconstructable
/// afterwards even when the base froze earlier and the transcripts are gone.
#[test]
fn record_then_apply_survives_transcript_loss() {
    let m = split("claude-opus-4-8", 100, 50, 2000, 500);
    let merged = base_with(&[("2026-06-16", m)]);

    let mut ledger = Ledger::default();
    assert!(ledger.record_covering(&merged, "2026-06-18", true));
    assert_eq!(ledger.recorded_through.as_deref(), Some("2026-06-17"));

    // Fresh base as if stats-cache is frozen at 06-09 and 06-16's transcripts
    // have been pruned — the ledger is the only surviving record.
    let mut fresh = TokenStats::default();
    ledger.apply_to_base(&mut fresh, Some("2026-06-09"));

    assert_eq!(fresh.total_input, 100);
    assert_eq!(fresh.total_output, 50);
    assert_eq!(fresh.total_cache_read, 2000);
    assert_eq!(fresh.total_cache_create, 500);
    assert_eq!(fresh.daily.len(), 1);
    assert_eq!(fresh.daily[0].date, "2026-06-16");
    assert_eq!(fresh.daily[0].tokens, 150, "daily is in+out");
    assert_eq!(fresh.daily_models.len(), 1);
    let row = &fresh.daily_models[0];
    assert_eq!(row.model, "claude-opus-4-8");
    assert_eq!(row.in_out, 150);
    assert_eq!(row.split.as_ref().expect("split kept").cache_read, 2000);
    assert_eq!(fresh.models.len(), 1);
    assert_eq!(fresh.models[0].total(), 100 + 50 + 2000 + 500);
}

/// A day CC's own aggregation later catches up to (base advances past it) must
/// not be folded twice.
#[test]
fn apply_skips_days_covered_by_stats_cache() {
    let mut ledger = Ledger::default();
    ledger.record_covering(
        &base_with(&[("2026-06-16", split("claude-opus-4-8", 100, 50, 0, 0))]),
        "2026-06-18",
        true,
    );

    let mut fresh = TokenStats::default();
    ledger.apply_to_base(&mut fresh, Some("2026-06-16"));

    assert!(
        fresh.daily.is_empty(),
        "a day at/before lastComputedDate is the base's, never re-added"
    );
    assert!(fresh.models.is_empty());
    assert_eq!(fresh.total_input, 0);
}

// ── cutoff ────────────────────────────────────────────────────────────────────

#[test]
fn effective_cutoff_takes_the_later_boundary() {
    let mut l = Ledger::default();
    assert_eq!(
        l.effective_cutoff(Some("2026-06-09")).as_deref(),
        Some("2026-06-09"),
        "no ledger yet → the base date"
    );
    assert_eq!(l.effective_cutoff(None), None);

    l.record_covering(
        &base_with(&[("2026-06-16", split("m", 1, 0, 0, 0))]),
        "2026-06-18",
        true,
    );
    // recorded_through = 06-17.
    assert_eq!(
        l.effective_cutoff(Some("2026-06-09")).as_deref(),
        Some("2026-06-17"),
        "ledger past a frozen base bounds the sweep"
    );
    assert_eq!(
        l.effective_cutoff(Some("2026-07-01")).as_deref(),
        Some("2026-07-01"),
        "a base that advanced past the ledger wins"
    );
    assert_eq!(l.effective_cutoff(None).as_deref(), Some("2026-06-17"));
}

// ── record boundaries ──────────────────────────────────────────────────────────

#[test]
fn record_never_stores_today() {
    let today = "2026-06-17";
    let merged = base_with(&[
        ("2026-06-16", split("m", 10, 0, 0, 0)),
        (today, split("m", 999, 0, 0, 0)),
    ]);

    let mut l = Ledger::default();
    assert!(l.record_covering(&merged, today, true));
    assert!(l.days.contains_key("2026-06-16"));
    assert!(
        !l.days.contains_key(today),
        "today is still being written — never finalized"
    );
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-16"));
}

#[test]
fn watermark_advances_across_idle_days_and_is_monotonic() {
    let mut l = Ledger::default();
    // A run with no usage still finalizes every day through yesterday.
    assert!(l.record_covering(&TokenStats::default(), "2026-06-20", true));
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-19"));
    assert!(l.days.is_empty());

    // A later day already at/before the watermark records nothing and never
    // regresses the watermark.
    let old = base_with(&[("2026-06-10", split("m", 5, 0, 0, 0))]);
    assert!(!l.record_covering(&old, "2026-06-20", true));
    assert!(l.days.is_empty());
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-19"));
}

// ── persistence ────────────────────────────────────────────────────────────────

#[test]
fn save_load_round_trip() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");

    let mut l = Ledger::default();
    l.record_covering(
        &base_with(&[("2026-06-16", split("claude-opus-4-8", 7, 3, 100, 20))]),
        "2026-06-18",
        true,
    );
    l.save(&dir);

    let reloaded = Ledger::load(&dir);
    assert_eq!(reloaded.recorded_through.as_deref(), Some("2026-06-17"));
    let mut fresh = TokenStats::default();
    reloaded.apply_to_base(&mut fresh, Some("2026-06-01"));
    assert_eq!(fresh.total_input, 7);
    assert_eq!(fresh.total_cache_read, 100);
}

// ── hourly axis (schema v2) ───────────────────────────────────────────────────

/// The v2 round-trip: per-hour buckets recorded from a merged base survive
/// save + load and land back on the pushed [`DayModelTokens`] row, so a day
/// recorded after the hourly axis prices peak/off-peak exactly.
#[test]
fn hours_survive_record_save_load_apply() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");

    let mut hours = [HourTokens::default(); 24];
    hours[5] = HourTokens {
        input: 40,
        output: 20,
        cache_read: 300,
        cache_create: 10,
        ..Default::default()
    };
    hours[23] = HourTokens {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_create: 0,
        ..Default::default()
    };
    let mut merged = TokenStats::default();
    merged.daily_models.push(DayModelTokens {
        date: "2026-06-16".into(),
        model: "claude-opus-4".into(),
        in_out: 150,
        split: Some(split("claude-opus-4", 100, 50, 2000, 500)),
        hours: Some(hours),
    });

    let mut ledger = Ledger::default();
    assert!(ledger.record_covering(&merged, "2026-06-18", true));
    // The wire model carries the buckets next to the flat split.
    let wm = &ledger.days["2026-06-16"]["claude-opus-4"];
    let wm_hours = wm.hours.as_ref().expect("hours recorded");
    assert_eq!(wm_hours[5].input, 40);
    assert_eq!(wm_hours[23].output, 1);

    ledger.save(&dir);
    let reloaded = Ledger::load(&dir);
    let mut fresh = TokenStats::default();
    reloaded.apply_to_base(&mut fresh, Some("2026-06-09"));
    assert_eq!(fresh.daily_models.len(), 1);
    let row = &fresh.daily_models[0];
    let restored = row.hours.expect("hours restored onto the pushed row");
    assert_eq!(restored[5].input, 40);
    assert_eq!(restored[5].cache_read, 300);
    assert_eq!(restored[23].input, 1);
    assert_eq!(restored[23].output, 1);
    assert_eq!(restored[0].output, 0, "unused slots stay empty");
    // The flat split is untouched by the new axis.
    assert_eq!(fresh.total_input, 100);
    assert_eq!(row.split.as_ref().expect("split").cache_read, 2000);

    // A replay after the watermark advanced skips the day entirely — its
    // hours are neither rewritten nor dropped.
    let mut replay = reloaded;
    assert!(
        !replay.record_covering(&fresh, "2026-06-18", true),
        "a day at/before the watermark is never re-recorded"
    );
    let wm = &replay.days["2026-06-16"]["claude-opus-4"];
    let wm_hours = wm.hours.as_ref().expect("hours survive a replay");
    assert_eq!(wm_hours[5].input, 40);
    assert_eq!(wm_hours[23].output, 1);
}

/// The serving-mode axis: a fast line's split is stored with its fast
/// sub-buckets and survives save + load + apply, so the day prices the fast
/// subset at the feed's fast rate.
#[test]
fn a_fast_line_split_survives_record_save_load() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");

    let mut hours = [HourTokens::default(); 24];
    hours[9] = HourTokens {
        input: 10,
        output: 20,
        cache_read: 5,
        cache_create: 2,
        fast_input: 10,
        fast_output: 20,
        fast_cache_read: 5,
        fast_cache_create: 2,
    };
    let mut merged = TokenStats::default();
    merged.daily_models.push(DayModelTokens {
        date: "2026-06-16".into(),
        model: "claude-opus-5-5".into(),
        in_out: 30,
        split: Some(ModelTokens {
            model: "claude-opus-5-5".into(),
            input: 10,
            output: 20,
            cache_read: 5,
            cache_create: 2,
            fast_input: 10,
            fast_output: 20,
            fast_cache_read: 5,
            fast_cache_create: 2,
            shape: UsageShape::Healthy,
        }),
        hours: Some(hours),
    });

    let mut ledger = Ledger::default();
    assert!(ledger.record_covering(&merged, "2026-06-18", true));
    ledger.save(&dir);

    let reloaded = Ledger::load(&dir);
    let w = &reloaded.days["2026-06-16"]["claude-opus-5-5"];
    let stored = w.hours.as_ref().expect("hourly axis stored")[9];
    assert_eq!((stored.input, stored.output), (10, 20));
    assert_eq!(
        (stored.fast_input, stored.fast_output),
        (10, 20),
        "the fast subset is stored beside the totals, not folded into them"
    );

    // And back out: the buckets re-apply onto the pushed rows.
    let mut fresh = TokenStats::default();
    reloaded.apply_to_base(&mut fresh, Some("2026-06-01"));
    let row = &fresh.daily_models[0];
    let h = &row.hours.as_ref().expect("hours survive the round trip")[9];
    assert_eq!((h.fast_input, h.fast_output), (10, 20));

    // The flat aggregate rides the same wire fast fields the buckets do: the
    // pushed row's split and the lifetime model row both carry the subset.
    let split = row.split.as_ref().expect("split");
    assert_eq!(
        (split.fast_input, split.fast_output),
        (10, 20),
        "the flat split's fast subset survives the round trip"
    );
    let m = fresh
        .models
        .iter()
        .find(|m| m.model == "claude-opus-5-5")
        .expect("lifetime model row");
    assert_eq!(
        (m.fast_input, m.fast_output),
        (10, 20),
        "the lifetime aggregate's fast subset survives the round trip"
    );
}

/// An old-format ledger (written before the fast sub-buckets existed) loads
/// whole and prices as before: flat fields intact, every fast bucket 0 —
/// those days price as the standard lines they were, never re-guessed. The
/// fixture is the byte-exact old wire shape: the serializer that wrote it
/// skipped the default `Healthy` shape, and the re-saved old day writes no
/// fast keys, so its wire shape is unchanged.
#[test]
fn an_old_ledger_without_fast_keys_loads_as_standard() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");

    let mut hour_cells = Vec::new();
    for i in 0..24 {
        hour_cells.push(if i == 9 {
            serde_json::json!({"input": 10, "output": 20, "cache_read": 5, "cache_create": 2})
        } else {
            serde_json::json!({"input": 0, "output": 0, "cache_read": 0, "cache_create": 0})
        });
    }
    let raw = serde_json::json!({
        "recorded_through": "2026-06-17",
        "days": {"2026-06-16": {"claude-opus-5-5": {
            "input": 10, "output": 20, "cache_read": 5, "cache_create": 2,
            "hours": hour_cells,
        }}},
        "backfill_done": true,
        "shape_clf": 3,
        "model_map": 1,
    });
    std::fs::write(
        dir.join("token_ledger.json"),
        serde_json::to_vec(&raw).expect("serialize fixture"),
    )
    .expect("write ledger fixture");

    let reloaded = Ledger::load(&dir);
    let w = &reloaded.days["2026-06-16"]["claude-opus-5-5"];
    assert_eq!(
        (w.input, w.output, w.cache_read, w.cache_create),
        (10, 20, 5, 2)
    );
    let h = w.hours.as_ref().expect("the hourly axis loads")[9];
    assert_eq!(
        (h.input, h.output, h.cache_read, h.cache_create),
        (10, 20, 5, 2)
    );
    assert_eq!(
        (
            h.fast_input,
            h.fast_output,
            h.fast_cache_read,
            h.fast_cache_create
        ),
        (0, 0, 0, 0),
        "a day recorded before the mode axis carries no fast tokens"
    );

    // The old day's wire shape is unchanged by a save: no fast keys appear.
    reloaded.save(&dir);
    let resaved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("token_ledger.json")).expect("read ledger"),
    )
    .expect("parse ledger");
    let cell = &resaved["days"]["2026-06-16"]["claude-opus-5-5"]["hours"][9];
    assert!(
        cell.get("fast_input").is_none(),
        "no fast keys on an old day"
    );
    assert!(
        cell.get("fast_output").is_none(),
        "no fast keys on an old day"
    );
    assert_eq!(cell["input"], 10);
    assert_eq!(cell["output"], 20);
}

/// A v1-shaped ledger (no `hours` keys) loads with `None`, applies its flat
/// data intact, and survives a save + re-record of a later day: the v1 day is
/// at/before the watermark, so it is never re-recorded and keeps its v1 wire
/// shape — no `hours` key appears for it even after the file is rewritten.
#[test]
fn v1_ledger_without_hours_loads_applies_and_rerecords() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");
    std::fs::write(
        dir.join("token_ledger.json"),
        r#"{
            "recorded_through": "2026-06-16",
            "days": {
                "2026-06-16": {
                    "claude-opus-4": {
                        "input": 100, "output": 50,
                        "cache_read": 2000, "cache_create": 500
                    }
                }
            }
        }"#,
    )
    .expect("write v1 ledger");

    let ledger = Ledger::load(&dir);
    assert_eq!(ledger.recorded_through.as_deref(), Some("2026-06-16"));

    let mut fresh = TokenStats::default();
    ledger.apply_to_base(&mut fresh, Some("2026-06-01"));
    assert_eq!(fresh.total_input, 100);
    assert_eq!(fresh.total_cache_read, 2000);
    assert_eq!(fresh.daily.len(), 1);
    assert_eq!(fresh.daily[0].tokens, 150);
    assert_eq!(fresh.daily_models.len(), 1);
    let row = &fresh.daily_models[0];
    assert!(row.split.is_some());
    assert!(row.hours.is_none(), "a v1 day carries no hourly axis");

    // A later run records a new day beside it: the v1 day is at/before the
    // watermark, so it is never re-recorded — its flat data and wire shape
    // survive while the new day gains the hourly axis.
    let mut merged = fresh;
    let mut h17 = [HourTokens::default(); 24];
    h17[7] = HourTokens {
        input: 6,
        output: 4,
        cache_read: 0,
        cache_create: 0,
        ..Default::default()
    };
    merged.daily_models.push(DayModelTokens {
        date: "2026-06-17".into(),
        model: "gpt-5".into(),
        in_out: 10,
        split: Some(split("gpt-5", 6, 4, 0, 0)),
        hours: Some(h17),
    });
    let mut ledger = ledger;
    assert!(ledger.record_covering(&merged, "2026-06-18", true));
    ledger.save(&dir);

    // On the wire the v1 day still has no `hours` key; the new day does.
    let raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join("token_ledger.json")).expect("read ledger"),
    )
    .expect("parse ledger");
    assert!(
        raw["days"]["2026-06-16"]["claude-opus-4"]
            .get("hours")
            .is_none(),
        "a v1 day must not gain an hours key on save"
    );
    assert_eq!(raw["days"]["2026-06-17"]["gpt-5"]["hours"][7]["input"], 6);

    // And the reloaded ledger still applies both days: v1 flat, v2 hourly.
    let reloaded = Ledger::load(&dir);
    let mut fresh2 = TokenStats::default();
    reloaded.apply_to_base(&mut fresh2, Some("2026-06-01"));
    assert_eq!(fresh2.total_input, 106);
    let v1_row = fresh2
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-16")
        .expect("v1 day");
    assert!(v1_row.hours.is_none());
    let v2_row = fresh2
        .daily_models
        .iter()
        .find(|d| d.date == "2026-06-17")
        .expect("v2 day");
    assert_eq!(v2_row.hours.expect("hours")[7].input, 6);
    assert_eq!(v2_row.hours.expect("hours")[7].output, 4);
}

/// A missing ledger is an empty one, never an error.
#[test]
fn load_missing_is_empty() {
    let sb = crate::testutil::HomeSandbox::new();
    let l = Ledger::load(&sb.home().join(".clauth"));
    assert!(l.recorded_through.is_none());
    assert!(l.days.is_empty());
}

// ── backfill (slice E) ─────────────────────────────────────────────────────────

/// A v1 ledger file (no `backfill_done` key) loads with the flag `false`, so
/// the one-shot hourly backfill is still owed — the serde default is what
/// makes every pre-upgrade ledger backfill exactly once.
#[test]
fn v1_ledger_without_flag_loads_backfill_pending() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).expect("mkdir .clauth");
    std::fs::write(
        dir.join("token_ledger.json"),
        r#"{
            "recorded_through": "2026-06-16",
            "days": {
                "2026-06-15": {
                    "claude-opus-4": {
                        "input": 100, "output": 50,
                        "cache_read": 20, "cache_create": 10
                    }
                }
            }
        }"#,
    )
    .expect("write v1 ledger");

    let ledger = Ledger::load(&dir);
    assert!(!ledger.backfill_done, "an absent flag reads false");
    // A pre-today hours-less day arms the pass up to the watermark.
    assert_eq!(
        ledger.backfill_through("2026-06-20").as_deref(),
        Some("2026-06-16")
    );
    // Only days strictly before `today` arm it.
    assert_eq!(ledger.backfill_through("2026-06-15").as_deref(), None);

    // The flag survives a save/reload round trip.
    ledger.save(&dir);
    assert!(!Ledger::load(&dir).backfill_done);

    // A pre-today day whose rows ALL carry hours does not arm the pass
    // either — the sweep could fill nothing.
    let mut done = Ledger {
        recorded_through: Some("2026-06-16".into()),
        ..Default::default()
    };
    done.days.insert(
        "2026-06-15".into(),
        HashMap::from([(
            "claude-opus-4".into(),
            WireModel {
                input: 100,
                output: 50,
                cache_read: 20,
                cache_create: 10,
                shape: Default::default(),
                hours: Some(
                    [WireHour {
                        input: 100,
                        output: 50,
                        cache_read: 20,
                        cache_create: 10,
                        ..Default::default()
                    }; 24],
                ),
                ..Default::default()
            },
        )]),
    );
    assert_eq!(
        done.backfill_through("2026-06-20"),
        None,
        "a fully-hours pre-today day does not arm the pass"
    );
    // And an empty ledger never does.
    assert_eq!(
        Ledger::default().backfill_through("2026-06-20"),
        None,
        "an empty ledger does not arm the pass"
    );
}

/// The fill rule, exact-or-absent: a row gains hour buckets only when the
/// re-derived flat totals equal the stored ones on all four fields. A
/// mismatching row (pruned transcripts) keeps `None` and its stored flat
/// totals — v1 data is never lowered — so a day whose rows do not all fill
/// keeps its v1 shape forever. The flag is set either way, so the one-shot
/// corpus sweep never re-runs.
#[test]
fn backfill_hours_fills_only_exact_matches() {
    // A v2-shaped row whose stored buckets differ from the derived ones, to
    // pin the never-rewrite guard.
    let mut v2h = [WireHour {
        input: 0,
        output: 0,
        cache_read: 0,
        cache_create: 0,
        ..Default::default()
    }; 24];
    v2h[3] = WireHour {
        input: 1,
        output: 1,
        cache_read: 1,
        cache_create: 1,
        ..Default::default()
    };
    let mut ledger = Ledger {
        recorded_through: Some("2026-06-16".into()),
        ..Default::default()
    };
    ledger.days.insert(
        "2026-06-15".into(),
        HashMap::from([
            (
                "claude-opus-4".into(),
                WireModel {
                    input: 100,
                    output: 50,
                    cache_read: 20,
                    cache_create: 10,
                    shape: Default::default(),
                    hours: None,
                    ..Default::default()
                },
            ),
            (
                "gpt-5".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    shape: Default::default(),
                    hours: None,
                    ..Default::default()
                },
            ),
            // One stored row per equality field: each derived twin below
            // matches on three fields and misses exactly one, so dropping
            // that field's comparison alone must leave the row v1.
            (
                "gpt-out".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    shape: Default::default(),
                    hours: None,
                    ..Default::default()
                },
            ),
            (
                "gpt-cr".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    shape: Default::default(),
                    hours: None,
                    ..Default::default()
                },
            ),
            (
                "gpt-cc".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    shape: Default::default(),
                    hours: None,
                    ..Default::default()
                },
            ),
            (
                "v2-model".into(),
                WireModel {
                    input: 9,
                    output: 9,
                    cache_read: 0,
                    cache_create: 0,
                    shape: Default::default(),
                    hours: Some(v2h),
                    ..Default::default()
                },
            ),
        ]),
    );

    let mut h09 = [HourTokens::default(); 24];
    h09[9] = HourTokens {
        input: 100,
        output: 50,
        cache_read: 20,
        cache_create: 10,
        ..Default::default()
    };
    let mut h07 = [HourTokens::default(); 24];
    h07[7] = HourTokens {
        input: 9,
        output: 9,
        cache_read: 0,
        cache_create: 0,
        ..Default::default()
    };
    let mut derived: HashMap<(String, String), crate::tokens::ModelDayAcc> = HashMap::new();
    // Exact match on all four fields: fills.
    derived.insert(
        ("2026-06-15".into(), "claude-opus-4".into()),
        crate::tokens::ModelDayAcc {
            flat: split("claude-opus-4", 100, 50, 20, 10),
            hours: h09,
        },
    );
    // Input off by one (6 vs 7): must NOT fill.
    derived.insert(
        ("2026-06-15".into(), "gpt-5".into()),
        crate::tokens::ModelDayAcc {
            flat: split("gpt-5", 6, 3, 0, 0),
            hours: [HourTokens::default(); 24],
        },
    );
    // Output off by one (4 vs 3): must NOT fill.
    derived.insert(
        ("2026-06-15".into(), "gpt-out".into()),
        crate::tokens::ModelDayAcc {
            flat: split("gpt-out", 7, 4, 0, 0),
            hours: [HourTokens::default(); 24],
        },
    );
    // cache_read off by five (5 vs 0): must NOT fill.
    derived.insert(
        ("2026-06-15".into(), "gpt-cr".into()),
        crate::tokens::ModelDayAcc {
            flat: split("gpt-cr", 7, 3, 5, 0),
            hours: [HourTokens::default(); 24],
        },
    );
    // cache_create off by five (5 vs 0): must NOT fill.
    derived.insert(
        ("2026-06-15".into(), "gpt-cc".into()),
        crate::tokens::ModelDayAcc {
            flat: split("gpt-cc", 7, 3, 0, 5),
            hours: [HourTokens::default(); 24],
        },
    );
    // Equal flats on a row that ALREADY carries hours, with different
    // buckets: the never-rewrite guard must keep the stored hours.
    derived.insert(
        ("2026-06-15".into(), "v2-model".into()),
        crate::tokens::ModelDayAcc {
            flat: split("v2-model", 9, 9, 0, 0),
            hours: h07,
        },
    );
    // A day the ledger does not hold: ignored.
    derived.insert(
        ("2026-06-14".into(), "m".into()),
        crate::tokens::ModelDayAcc {
            flat: split("m", 1, 1, 0, 0),
            hours: [HourTokens::default(); 24],
        },
    );

    ledger.backfill_hours(&derived);

    assert!(
        ledger.backfill_done,
        "the flag is set even when rows did not fill"
    );
    let opus = &ledger.days["2026-06-15"]["claude-opus-4"];
    assert_eq!(opus.input, 100, "flat totals never rewritten");
    assert_eq!(opus.output, 50);
    assert_eq!(opus.cache_read, 20);
    assert_eq!(opus.cache_create, 10);
    let hours = opus.hours.as_ref().expect("exact match fills");
    assert_eq!(hours[9].input, 100);
    assert_eq!(hours[9].output, 50);
    assert_eq!(hours[9].cache_read, 20);
    assert_eq!(hours[9].cache_create, 10);
    assert_eq!(hours[0].output, 0);

    for model in ["gpt-5", "gpt-out", "gpt-cr", "gpt-cc"] {
        let row = &ledger.days["2026-06-15"][model];
        assert!(
            row.hours.is_none(),
            "{model}: a one-field mismatch leaves the row v1"
        );
        assert_eq!(row.input, 7, "{model}: mismatched flat totals untouched");
    }

    let v2 = &ledger.days["2026-06-15"]["v2-model"];
    let stored = v2.hours.as_ref().expect("stored hours survive");
    assert_eq!(stored[3].input, 1, "the stored buckets are not rewritten");
    assert_eq!(stored[7].input, 0, "the derived buckets do not land");
    assert!(
        !ledger.days.contains_key("2026-06-14"),
        "a derived day the ledger does not hold is ignored"
    );
}

// ── served-model re-derive ─────────────────────────────────────────────────

fn wire(input: u64, output: u64, cache_read: u64, cache_create: u64) -> WireModel {
    WireModel {
        input,
        output,
        cache_read,
        cache_create,
        shape: Default::default(),
        hours: None,
        ..Default::default()
    }
}

fn acc(model: &str, values: (u64, u64, u64, u64), hour: usize) -> crate::tokens::ModelDayAcc {
    let (input, output, cache_read, cache_create) = values;
    let mut hours = [HourTokens::default(); 24];
    hours[hour] = HourTokens {
        input,
        output,
        cache_read,
        cache_create,
        ..Default::default()
    };
    crate::tokens::ModelDayAcc {
        flat: split(model, input, output, cache_read, cache_create),
        hours,
    }
}

/// The movable-key gate: only keys the served-model map can move (`claude-*`
/// and the models it names) take part. A day whose re-derived movable sums
/// equal the stored ones on all four fields gets its movable rows replaced by
/// the re-derived split; every other row stays as recorded, a pruned one
/// included. A day short or over on any field keeps its recorded rows, and so
/// does a day the map leaves unchanged. The version stamps either way, so the
/// pass never re-arms.
#[test]
fn remap_models_rekeys_only_the_movable_rows_the_corpus_fully_covers() {
    let mut ledger = Ledger {
        recorded_through: Some("2026-06-16".into()),
        ..Default::default()
    };
    let opus = |row: WireModel| HashMap::from([("claude-opus-5-5".to_owned(), row)]);
    let days = [
        ("2026-06-15", opus(wire(107, 53, 10, 4))),
        ("2026-06-14", opus(wire(12, 5, 0, 0))),
        ("2026-06-13", opus(wire(10, 10, 10, 10))),
        ("2026-06-12", opus(wire(10, 10, 10, 10))),
        ("2026-06-11", opus(wire(10, 10, 10, 10))),
        ("2026-06-10", opus(wire(1, 1, 0, 0))),
        (
            "2026-06-08",
            HashMap::from([
                ("claude-opus-5-5".to_owned(), wire(20, 8, 0, 0)),
                ("qwen/qwen3.6-27b".to_owned(), wire(52_109, 0, 0, 0)),
            ]),
        ),
        ("2026-06-07", opus(wire(10, 10, 0, 0))),
        ("2026-06-06", opus(wire(4, 4, 0, 0))),
        (
            "2026-06-05",
            HashMap::from([("claude-sonnet-5-5".to_owned(), wire(6, 2, 0, 0))]),
        ),
    ];
    for (date, rows) in days {
        ledger.days.insert(date.into(), rows);
    }
    let derived: HashMap<(String, String), crate::tokens::ModelDayAcc> = [
        // Covered: one stored row splits across the anthropic and the
        // DeepSeek-served model; a model the map cannot move rides along.
        ("2026-06-15", "claude-opus-5-5", (100, 50, 10, 4), 10),
        ("2026-06-15", "deepseek-v4-pro", (7, 3, 0, 0), 11),
        ("2026-06-15", "glm-5.3", (1, 1, 0, 0), 11),
        // One field short each: input, output, cache-read, cache-create.
        ("2026-06-14", "deepseek-v4-pro", (11, 5, 0, 0), 9),
        ("2026-06-13", "deepseek-v4-pro", (10, 9, 10, 10), 9),
        ("2026-06-12", "deepseek-v4-pro", (10, 10, 9, 10), 9),
        ("2026-06-11", "deepseek-v4-pro", (10, 10, 10, 9), 9),
        // Covered on its movable rows; the input-only row beside them is
        // pruned from the corpus and must survive.
        ("2026-06-08", "deepseek-v4-pro", (20, 8, 0, 0), 9),
        // The corpus holds more than the stored day.
        ("2026-06-07", "deepseek-v4-pro", (10, 10, 0, 0), 9),
        ("2026-06-07", "claude-opus-5-5", (5, 5, 0, 0), 9),
        // Unchanged by the map: the stored row is kept, hours and all.
        ("2026-06-06", "claude-opus-5-5", (4, 4, 0, 0), 9),
        // Covered: a sonnet row moves onto the other served model.
        ("2026-06-05", "deepseek-flash", (6, 2, 0, 0), 9),
        // A day the ledger does not hold: ignored.
        ("2026-06-09", "deepseek-v4-pro", (5, 5, 0, 0), 9),
    ]
    .into_iter()
    .map(|(date, model, values, hour)| ((date.into(), model.into()), acc(model, values, hour)))
    .collect();

    assert_eq!(
        ledger.remap_through("2026-06-20"),
        Some("2026-06-16".into())
    );
    ledger.remap_models(&derived);

    let fields = |date: &str, model: &str| ledger.wire_model_fields(date, model);
    assert_eq!(
        fields("2026-06-15", "claude-opus-5-5"),
        Some((100, 50, 10, 4, Default::default(), true))
    );
    assert_eq!(
        fields("2026-06-15", "deepseek-v4-pro"),
        Some((7, 3, 0, 0, Default::default(), true))
    );
    assert_eq!(
        ledger.days["2026-06-15"].len(),
        2,
        "a re-derived row the map cannot move never lands"
    );
    assert_eq!(
        ledger.days["2026-06-15"]["deepseek-v4-pro"]
            .hours
            .expect("hours")[11]
            .output,
        3
    );
    for date in ["2026-06-14", "2026-06-13", "2026-06-12", "2026-06-11"] {
        assert_eq!(
            fields(date, "deepseek-v4-pro"),
            None,
            "{date}: one field short"
        );
    }
    assert_eq!(
        fields("2026-06-14", "claude-opus-5-5"),
        Some((12, 5, 0, 0, Default::default(), false))
    );
    assert_eq!(
        fields("2026-06-13", "claude-opus-5-5"),
        Some((10, 10, 10, 10, Default::default(), false))
    );
    assert_eq!(
        fields("2026-06-08", "deepseek-v4-pro"),
        Some((20, 8, 0, 0, Default::default(), true))
    );
    assert_eq!(fields("2026-06-08", "claude-opus-5-5"), None);
    assert_eq!(
        fields("2026-06-08", "qwen/qwen3.6-27b"),
        Some((52_109, 0, 0, 0, Default::default(), false)),
        "a row the map cannot move stays as recorded"
    );
    assert_eq!(
        fields("2026-06-07", "claude-opus-5-5"),
        Some((10, 10, 0, 0, Default::default(), false))
    );
    assert_eq!(
        fields("2026-06-07", "deepseek-v4-pro"),
        None,
        "more than stored is no match"
    );
    assert_eq!(
        fields("2026-06-06", "claude-opus-5-5"),
        Some((4, 4, 0, 0, Default::default(), false)),
        "a day the map leaves unchanged is not rewritten"
    );
    assert_eq!(
        fields("2026-06-10", "claude-opus-5-5"),
        Some((1, 1, 0, 0, Default::default(), false)),
        "a day the corpus never reaches stays as recorded"
    );
    assert_eq!(
        fields("2026-06-05", "deepseek-flash"),
        Some((6, 2, 0, 0, Default::default(), true))
    );
    assert_eq!(fields("2026-06-05", "claude-sonnet-5-5"), None);
    assert!(!ledger.days.contains_key("2026-06-09"));
    assert_eq!(
        ledger.remap_through("2026-06-20"),
        None,
        "the pass stamps done"
    );
}

/// A ledger this build starts records every day through the current map, so
/// it owes no re-key and its first tick sweeps nothing for one.
#[test]
fn a_ledger_this_build_starts_owes_no_remap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut ledger = Ledger::load(dir.path());
    ledger.record_covering(
        &base_with(&[("2026-06-15", split("claude-opus-5-5", 1, 1, 0, 0))]),
        "2026-06-20",
        true,
    );
    assert_eq!(ledger.remap_through("2026-06-20"), None);
}

/// A ledger written before the served-model map owes exactly one pass, and
/// the stamp survives a save/load round trip.
#[test]
fn a_pre_map_ledger_owes_one_remap_and_the_stamp_persists() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("token_ledger.json"),
        r#"{"recorded_through":"2026-06-16","days":{"2026-06-15":{"m":{"input":1,"output":1,"cache_read":0,"cache_create":0}}}}"#,
    )
    .expect("write");
    let mut ledger = Ledger::load(dir.path());
    assert_eq!(
        ledger.remap_through("2026-06-20"),
        Some("2026-06-16".into())
    );
    assert_eq!(
        ledger.remap_through("2026-06-15"),
        None,
        "no day before today"
    );
    ledger.remap_models(&HashMap::new());
    ledger.save(dir.path());
    assert_eq!(Ledger::load(dir.path()).remap_through("2026-06-20"), None);
}

// ── tps speed ledger (token_speed.json) ───────────────────────────────────────

/// Speed history lives in its own file: a pre-migration token ledger loads and
/// stays byte-identical through a speed fold, the producer's finalized day sums
/// land in `token_speed.json` under (day, account, model) with today's left
/// out, and a second fold changes nothing.
#[test]
fn a_pre_migration_ledger_loads_and_speed_rows_fold_into_their_own_file() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let v1 = r#"{"recorded_through":"2026-10-13","days":{"2026-10-13":{"claude-opus-5-5":{"input":1,"output":2,"cache_read":3,"cache_create":4}}},"backfill_done":true,"shape_clf":3,"model_map":1}"#;
    std::fs::write(dir.join("token_ledger.json"), v1).unwrap();

    let l = Ledger::load(&dir);
    assert_eq!(l.recorded_through.as_deref(), Some("2026-10-13"));
    assert_eq!(l.days["2026-10-13"]["claude-opus-5-5"].output, 2);

    let mut days = crate::tps::DaySums::new();
    for (date, account, output, gen_ms) in [
        ("2026-10-14", "kerry", 1222, 13_597),
        ("2026-10-14", "*", 300, 4_000),
        ("2026-10-15", "kerry", 50, 1_000),
    ] {
        days.entry(date.into())
            .or_default()
            .entry(account.into())
            .or_default()
            .insert(
                "claude-opus-5-5".into(),
                crate::tps::DaySum {
                    output,
                    gen_ms,
                    ..Default::default()
                },
            );
    }
    let mut speed = SpeedLedger::load(&dir);
    assert_eq!(
        speed.fold(&dir, &days, &BTreeMap::new(), "2026-10-15"),
        FoldOutcome::Saved,
        "a finalized day folds"
    );
    assert_eq!(
        speed.fold(&dir, &days, &BTreeMap::new(), "2026-10-15"),
        FoldOutcome::Unchanged,
        "a second fold is a no-op"
    );

    assert_eq!(
        std::fs::read_to_string(dir.join("token_ledger.json")).unwrap(),
        v1,
        "the token ledger is never written by a speed fold"
    );
    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("token_speed.json")).unwrap()).unwrap();
    assert_eq!(
        raw,
        serde_json::json!({"version": 2, "days": {"2026-10-14": {
            "*": {"claude-opus-5-5": {"output": 300, "gen_ms": 4000}},
            "kerry": {"claude-opus-5-5": {"output": 1222, "gen_ms": 13597}},
        }}}),
        "only the finalized day, keyed day -> account -> model, unattributed under *"
    );
    assert_eq!(
        SpeedLedger::load(&dir).days["2026-10-14"]["kerry"]["claude-opus-5-5"],
        crate::tps::DaySum {
            output: 1222,
            gen_ms: 13_597,
            ..Default::default()
        },
        "the speed ledger loads back"
    );
}

/// An unparsable `token_speed.json` is renamed aside before the first save,
/// never saved over (its rows have no second source).
#[test]
fn an_unparsable_speed_ledger_is_renamed_aside_not_saved_over() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("token_speed.json"), "not json").unwrap();

    let mut l = SpeedLedger::load(&dir);
    let mut days = crate::tps::DaySums::new();
    days.entry("2026-09-20".to_owned())
        .or_default()
        .entry("kerry".to_owned())
        .or_default()
        .insert(
            "claude-opus-5-5".to_owned(),
            crate::tps::DaySum {
                output: 1,
                gen_ms: 2,
                ..Default::default()
            },
        );
    assert_eq!(
        l.fold(&dir, &days, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved,
        "the fresh save succeeds"
    );

    let aside = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
    assert!(aside, "the corrupt file was renamed aside");
    let reloaded = SpeedLedger::load(&dir);
    assert!(
        reloaded.days.contains_key("2026-09-20"),
        "the fresh file holds the folded day"
    );
}

/// A read error other than NotFound (a directory at the file's path) marks the
/// ledger unreadable: it is never saved over, and the days stay in the cache.
#[test]
fn an_unreadable_speed_ledger_is_never_saved_over() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir(dir.join("token_speed.json")).unwrap();

    let mut l = SpeedLedger::load(&dir);
    assert!(
        l.unusable(),
        "a directory read error is unreadable, not absent"
    );
    let mut days = crate::tps::DaySums::new();
    days.entry("2026-09-20".to_owned())
        .or_default()
        .entry("kerry".to_owned())
        .or_default()
        .insert(
            "claude-opus-5-5".to_owned(),
            crate::tps::DaySum {
                output: 1,
                gen_ms: 2,
                ..Default::default()
            },
        );
    assert_eq!(
        l.fold(&dir, &days, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Unsaved,
        "a fold never lands over an unreadable file"
    );
    assert!(!l.save(&dir), "never saved over an unreadable file");
    assert!(
        dir.join("token_speed.json").is_dir(),
        "the unreadable file is untouched"
    );
}

/// Two saves after one corruption make exactly one aside (the flag clears on a
/// successful rename) and the aside is 0600.
#[test]
fn a_corrupt_speed_ledger_is_renamed_aside_exactly_once() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("token_speed.json"), "not json").unwrap();

    let mut l = SpeedLedger::load(&dir);
    let mut days = crate::tps::DaySums::new();
    days.entry("2026-09-20".to_owned())
        .or_default()
        .entry("kerry".to_owned())
        .or_default()
        .insert(
            "claude-opus-5-5".to_owned(),
            crate::tps::DaySum {
                output: 1,
                gen_ms: 2,
                ..Default::default()
            },
        );
    assert_eq!(
        l.fold(&dir, &days, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    assert!(l.save(&dir), "the second save needs no rename");

    let asides: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("corrupt"))
        .collect();
    assert_eq!(asides.len(), 1, "exactly one aside");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(asides[0].path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the aside is 0600");
    }
}

/// A v1 `token_speed.json` (no `speed` on any cell, no `rate_peaks` key) loads
/// with every peak absent and is saved back as version 2, keeping its day
/// sums byte-for-byte.
#[test]
fn a_v1_speed_ledger_loads_and_saves_back_as_v2() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("token_speed.json"),
        r#"{"version":1,"days":{"2026-10-14":{"kerry":{"claude-opus-5-5":{"output":1222,"gen_ms":13597}}}}}"#,
    )
    .unwrap();

    let mut speed = SpeedLedger::load(&dir);
    assert_eq!(
        speed.days["2026-10-14"]["kerry"]["claude-opus-5-5"],
        crate::tps::DaySum {
            output: 1222,
            gen_ms: 13_597,
            ..Default::default()
        },
        "the v1 cell loads"
    );
    assert!(
        speed.days["2026-10-14"]["kerry"]["claude-opus-5-5"]
            .speed
            .is_none(),
        "a v1 cell has no peak"
    );
    assert!(speed.rate_peaks.is_empty(), "a v1 file has no rate peaks");
    assert!(speed.save(&dir), "the v1 file is saved back");

    let raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("token_speed.json")).unwrap()).unwrap();
    assert_eq!(raw["version"], 2, "saved as v2: {raw}");
    assert_eq!(
        raw["days"]["2026-10-14"]["kerry"]["claude-opus-5-5"]["output"], 1222,
        "the sums survive: {raw}"
    );
    assert!(raw.get("rate_peaks").is_none(), "no rate peaks yet: {raw}");
}

/// A version 3 speed ledger reads as newer (3 > 2): never saved over, never
/// retry-reloaded, and its days are left untouched on disk.
#[test]
fn a_newer_speed_ledger_is_never_saved_over() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("token_speed.json"), r#"{"version":3,"days":{}}"#).unwrap();

    let mut speed = SpeedLedger::load(&dir);
    assert!(speed.newer(), "v3 is newer than v2");
    assert!(speed.unusable(), "a newer ledger is never saved over");
    assert!(!speed.save(&dir), "save refuses a newer ledger");
    assert_eq!(
        std::fs::read_to_string(dir.join("token_speed.json")).unwrap(),
        r#"{"version":3,"days":{}}"#,
        "the newer file is untouched"
    );
}

/// A fold refused at its temp file (an unwritable `.clauth`) builds none of
/// the ledger's content and leaves the ledger as it was; the same fold lands
/// once the dir is writable, building it once. The count sees only
/// `SpeedLedger::content`: a serialize or clone of the ledger's days or rate
/// peaks outside it, before the temp opens, goes unseen.
#[test]
fn a_fold_refused_at_its_temp_file_builds_no_content() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let mut l = SpeedLedger::load(&dir);
    let mut days = crate::tps::DaySums::new();
    days.entry("2026-09-20".to_owned())
        .or_default()
        .entry("kerry".to_owned())
        .or_default()
        .insert(
            "claude-opus-5-5".to_owned(),
            crate::tps::DaySum {
                output: 1,
                gen_ms: 2,
                ..Default::default()
            },
        );
    let unwritable = crate::testutil::UnwritableDir::new(&dir);
    let before = serializations::taken();
    for _ in 0..3 {
        assert_eq!(
            l.fold(&dir, &days, &BTreeMap::new(), "2026-09-21"),
            FoldOutcome::Unsaved
        );
    }
    assert_eq!(
        serializations::taken() - before,
        0,
        "a refused fold built the ledger's content"
    );
    assert!(l.days.is_empty(), "a refused fold left the ledger merged");
    drop(unwritable);
    assert_eq!(
        l.fold(&dir, &days, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    assert_eq!(serializations::taken() - before, 1);
    assert_eq!(SpeedLedger::load(&dir).days, days, "the landed fold");
}

/// A speed cell on one model: `(account, output, gen_ms, peak)`, the peak as
/// `(value, at)`.
type SpeedCell<'a> = (&'a str, u64, u64, Option<(f64, u64)>);

/// One day of [`SpeedCell`]s.
fn speed_day(date: &str, cells: &[SpeedCell<'_>]) -> crate::tps::DaySums {
    let mut days = crate::tps::DaySums::new();
    for &(account, output, gen_ms, peak) in cells {
        days.entry(date.to_owned())
            .or_default()
            .entry(account.to_owned())
            .or_default()
            .insert(
                "claude-opus-5-5".to_owned(),
                crate::tps::DaySum {
                    output,
                    gen_ms,
                    speed: peak.map(|(value, at)| crate::tps::PeakAt { value, at }),
                },
            );
    }
    days
}

/// A save failing after its temp opens (a directory at `token_speed.json`
/// refuses the rename) builds its content once: every retry of the same
/// content only writes it again, the plain save and a fold asking in turn
/// each keeping their own. A fold of other days builds anew; the retry that
/// lands writes the kept bytes, and the next save, of the changed ledger,
/// builds again. The count sees only `SpeedLedger::content`: a serialize or
/// clone of the ledger's days or rate peaks outside it goes unseen.
#[test]
fn a_save_failing_after_its_temp_opens_builds_no_content_until_the_ledger_changes() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let mut l = SpeedLedger::load(&dir);
    let first = speed_day("2026-09-19", &[("kerry", 100, 1_000, None)]);
    assert_eq!(
        l.fold(&dir, &first, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    let target = dir.join("token_speed.json");
    std::fs::remove_file(&target).unwrap();
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep"), b"x").unwrap();
    let second = speed_day("2026-09-20", &[("kerry", 7, 70, None)]);
    let before = serializations::taken();
    for _ in 0..3 {
        assert!(!l.save(&dir), "the rename onto a directory fails");
        assert_eq!(
            l.fold(&dir, &second, &BTreeMap::new(), "2026-09-21"),
            FoldOutcome::Unsaved
        );
    }
    assert_eq!(
        serializations::taken() - before,
        2,
        "a retry of the same content rebuilt it"
    );
    let third = speed_day("2026-09-20", &[("kerry", 9, 90, None)]);
    for _ in 0..2 {
        assert_eq!(
            l.fold(&dir, &third, &BTreeMap::new(), "2026-09-21"),
            FoldOutcome::Unsaved
        );
    }
    assert_eq!(
        serializations::taken() - before,
        3,
        "a fold of other days builds its content once"
    );
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["token_speed.json"], "a temp stayed behind");
    std::fs::remove_dir_all(&target).unwrap();
    assert_eq!(
        l.fold(&dir, &third, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    assert_eq!(
        serializations::taken() - before,
        3,
        "the landing retry rebuilt the kept content"
    );
    let mut landed = first;
    landed.extend(third);
    assert_eq!(SpeedLedger::load(&dir).days, landed, "the landed fold");
    assert!(l.save(&dir));
    assert_eq!(
        serializations::taken() - before,
        4,
        "a save of the changed ledger builds its content"
    );
}

/// A refold whose model total ties the stored one takes the new account split
/// whole (a swap moved the day's responses from kerry to mommy): the day stays
/// 1000/10 s, never kerry 1000 + mommy 1000, and kerry's faster stored peak
/// stays on a cell with no sums. A later smaller total (a transcript lost)
/// leaves the stored split as it is, while its faster peak still lands.
#[test]
fn a_refold_takes_the_larger_model_total_with_its_split_and_keeps_every_peak() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let d = "2026-09-19";
    let mut l = SpeedLedger::load(&dir);
    let stored = speed_day(d, &[("kerry", 1000, 10_000, Some((50.0, 1)))]);
    assert_eq!(
        l.fold(&dir, &stored, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    let moved = speed_day(d, &[("mommy", 1000, 10_000, Some((40.0, 2)))]);
    assert_eq!(
        l.fold(&dir, &moved, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    assert_eq!(
        SpeedLedger::load(&dir).days,
        speed_day(
            d,
            &[
                ("kerry", 0, 0, Some((50.0, 1))),
                ("mommy", 1000, 10_000, Some((40.0, 2))),
            ]
        ),
        "the tied total's split replaced the stored one"
    );
    let smaller = speed_day(d, &[("kerry", 600, 6_000, Some((70.0, 3)))]);
    assert_eq!(
        l.fold(&dir, &smaller, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    assert_eq!(
        SpeedLedger::load(&dir).days,
        speed_day(
            d,
            &[
                ("kerry", 0, 0, Some((70.0, 3))),
                ("mommy", 1000, 10_000, Some((40.0, 2))),
            ]
        ),
        "a smaller total keeps the stored split, the faster peak lands"
    );
}

/// Model totals compare `(output, gen_ms)` in order: a refold with the stored
/// output (1000) over a shorter span (8 s against 10 s) is the smaller total,
/// so the stored kerry split stays and the fold changes nothing.
#[test]
fn a_refold_with_equal_output_and_a_shorter_span_keeps_the_stored_split() {
    let sb = crate::testutil::HomeSandbox::new();
    let dir = sb.home().join(".clauth");
    std::fs::create_dir_all(&dir).unwrap();
    let d = "2026-09-19";
    let mut l = SpeedLedger::load(&dir);
    let stored = speed_day(d, &[("kerry", 1000, 10_000, None)]);
    assert_eq!(
        l.fold(&dir, &stored, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Saved
    );
    let shorter = speed_day(d, &[("mommy", 1000, 8_000, None)]);
    assert_eq!(
        l.fold(&dir, &shorter, &BTreeMap::new(), "2026-09-21"),
        FoldOutcome::Unchanged,
        "the shorter span refolded the day"
    );
    assert_eq!(
        SpeedLedger::load(&dir).days,
        speed_day(d, &[("kerry", 1000, 10_000, None)]),
        "the stored split stays"
    );
}
