use super::*;

use crate::pricing::HourTokens;
use crate::tokens::{DayModelTokens, ModelTokens, TokenStats};

// ── helpers ──────────────────────────────────────────────────────────────────

fn split(model: &str, input: u64, output: u64, cache_read: u64, cache_create: u64) -> ModelTokens {
    ModelTokens {
        model: model.to_owned(),
        input,
        output,
        cache_read,
        cache_create,
    }
}

/// A merged base carrying transcript-derived (split-bearing) per-day rows — the
/// shape `merge_topup` hands to [`Ledger::record`]. The rows carry no hourly
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

// ── durability ────────────────────────────────────────────────────────────────

/// The core guarantee: a day recorded from transcripts is fully reconstructable
/// afterwards even when the base froze earlier and the transcripts are gone.
#[test]
fn record_then_apply_survives_transcript_loss() {
    let m = split("claude-opus-4-8", 100, 50, 2000, 500);
    let merged = base_with(&[("2026-06-16", m)]);

    let mut ledger = Ledger::default();
    assert!(ledger.record(&merged, "2026-06-18"));
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
    ledger.record(
        &base_with(&[("2026-06-16", split("claude-opus-4-8", 100, 50, 0, 0))]),
        "2026-06-18",
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

    l.record(
        &base_with(&[("2026-06-16", split("m", 1, 0, 0, 0))]),
        "2026-06-18",
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
    assert!(l.record(&merged, today));
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
    assert!(l.record(&TokenStats::default(), "2026-06-20"));
    assert_eq!(l.recorded_through.as_deref(), Some("2026-06-19"));
    assert!(l.days.is_empty());

    // A later day already at/before the watermark records nothing and never
    // regresses the watermark.
    let old = base_with(&[("2026-06-10", split("m", 5, 0, 0, 0))]);
    assert!(!l.record(&old, "2026-06-20"));
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
    l.record(
        &base_with(&[("2026-06-16", split("claude-opus-4-8", 7, 3, 100, 20))]),
        "2026-06-18",
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
    };
    hours[23] = HourTokens {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_create: 0,
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
    assert!(ledger.record(&merged, "2026-06-18"));
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
        !replay.record(&fresh, "2026-06-18"),
        "a day at/before the watermark is never re-recorded"
    );
    let wm = &replay.days["2026-06-16"]["claude-opus-4"];
    let wm_hours = wm.hours.as_ref().expect("hours survive a replay");
    assert_eq!(wm_hours[5].input, 40);
    assert_eq!(wm_hours[23].output, 1);
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
    };
    merged.daily_models.push(DayModelTokens {
        date: "2026-06-17".into(),
        model: "gpt-5".into(),
        in_out: 10,
        split: Some(split("gpt-5", 6, 4, 0, 0)),
        hours: Some(h17),
    });
    let mut ledger = ledger;
    assert!(ledger.record(&merged, "2026-06-18"));
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
                hours: Some(
                    [WireHour {
                        input: 100,
                        output: 50,
                        cache_read: 20,
                        cache_create: 10,
                    }; 24],
                ),
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
    }; 24];
    v2h[3] = WireHour {
        input: 1,
        output: 1,
        cache_read: 1,
        cache_create: 1,
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
                    hours: None,
                },
            ),
            (
                "gpt-5".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    hours: None,
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
                    hours: None,
                },
            ),
            (
                "gpt-cr".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    hours: None,
                },
            ),
            (
                "gpt-cc".into(),
                WireModel {
                    input: 7,
                    output: 3,
                    cache_read: 0,
                    cache_create: 0,
                    hours: None,
                },
            ),
            (
                "v2-model".into(),
                WireModel {
                    input: 9,
                    output: 9,
                    cache_read: 0,
                    cache_create: 0,
                    hours: Some(v2h),
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
    };
    let mut h07 = [HourTokens::default(); 24];
    h07[7] = HourTokens {
        input: 9,
        output: 9,
        cache_read: 0,
        cache_create: 0,
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
