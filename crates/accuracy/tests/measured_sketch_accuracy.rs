//! The measured accuracy table as Pass 2's [`AccuracyEvidence`]: a request
//! at failure probability δ takes the largest recorded δ' ≤ δ of an entry
//! with exactly its parameters, statistic and metric, and nothing else.
//! Every expected value is read off the fixture by hand.

use asap_accuracy::{
    theoretical_guarantee, AccuracyEvidence, ErrorAtFailureProbability, MeasuredAccuracyEntry,
    MeasuredSketchAccuracy,
};
use asap_types::accuracy::{AccuracyGuarantee, AccuracyMetric};
use asap_types::ir::schema::FieldDataType;
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};

fn kll() -> FieldDataType {
    FieldDataType::Sketch(SketchAlgorithm::Kll)
}

fn p99() -> SketchStatistic {
    SketchStatistic::Quantile { q: 0.99 }
}

fn point(failure_probability: f64, error: f64) -> ErrorAtFailureProbability {
    ErrorAtFailureProbability {
        failure_probability,
        error,
    }
}

/// KLL k = 547 read for the 0.99 quantile, rank error over 1,000 trials:
/// 0.0031 at δ = 0.003, 0.0024 at 0.01, 0.0016 at 0.05.
fn kll_547_entry() -> MeasuredAccuracyEntry {
    MeasuredAccuracyEntry {
        sketch: SketchParams::Kll { k: 547 },
        statistic: p99(),
        accuracy_metric: AccuracyMetric::Rank,
        trials: 1_000,
        error_at: vec![
            point(0.003, 0.0031),
            point(0.01, 0.0024),
            point(0.05, 0.0016),
        ],
    }
}

fn table() -> MeasuredSketchAccuracy {
    MeasuredSketchAccuracy {
        entries: vec![kll_547_entry()],
    }
}

/// The error, failure probability and metric of a guarantee.
fn parts(guarantee: &AccuracyGuarantee) -> (f64, f64, AccuracyMetric) {
    (
        guarantee.spec.error.evaluate().unwrap(),
        guarantee.spec.failure_probability.evaluate().unwrap(),
        guarantee.spec.accuracy_metric,
    )
}

/// δ = 0.01 takes 0.01's error; 0.02 takes 0.01's too (the largest
/// recorded δ' ≤ 0.02, not the nearest 0.05 above); 0.05 and 1.0 (no
/// requirement) take 0.05's.
#[test]
fn lookup_takes_largest_recorded_delta_at_most_request() {
    let table = table();
    let at = |delta| {
        parts(
            &table
                .measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), delta)
                .unwrap(),
        )
    };
    assert_eq!(at(0.01), (0.0024, 0.01, AccuracyMetric::Rank));
    assert_eq!(at(0.02), (0.0024, 0.01, AccuracyMetric::Rank));
    assert_eq!(at(0.05), (0.0016, 0.05, AccuracyMetric::Rank));
    assert_eq!(at(1.0), (0.0016, 0.05, AccuracyMetric::Rank));
}

/// δ = 0.002 is below every recorded δ': no measured guarantee, never
/// 0.0031 extrapolated downward.
#[test]
fn lookup_no_extrapolation_below_smallest() {
    let guarantee =
        table().measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), 0.002);
    assert_eq!(guarantee, None);
}

/// Where the table has no measurement (δ = 0.002 above), the theoretical
/// guarantee applies: KLL k = 547 has rank error 2.296 / 547^0.9723 =
/// 0.004998 at δ ≥ 0.01, scaled below 0.01 by z(1 − δ/2) / z(0.995):
/// 0.004998 · 3.0902 / 2.5758 = 0.005997 at δ = 0.002.
#[test]
fn closed_form_applies_below_the_measured_range() {
    let guarantee =
        theoretical_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), 0.002).unwrap();
    let (error, _, metric) = parts(&guarantee);
    assert_eq!(metric, AccuracyMetric::Rank);
    assert!((error - 0.005997).abs() < 5e-6, "{error}");
}

/// Only the exact parameters, statistic and metric match: k = 548, the
/// median, or the relative metric get nothing.
#[test]
fn lookup_exact_params_statistic_metric() {
    let table = table();
    assert_eq!(
        table.measured_guarantee(&kll(), &SketchParams::Kll { k: 548 }, &p99(), 0.01),
        None
    );
    assert_eq!(
        table.measured_guarantee(
            &kll(),
            &SketchParams::Kll { k: 547 },
            &SketchStatistic::Quantile { q: 0.5 },
            0.01
        ),
        None
    );
    let relative = MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            accuracy_metric: AccuracyMetric::Relative,
            ..kll_547_entry()
        }],
    };
    // provisional answer 7: an entry in another metric than the readout's
    // own (Rank for KLL) is never returned.
    assert_eq!(
        relative.measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), 0.01),
        None
    );
}

/// An empty table measures nothing.
#[test]
fn empty_table_measures_nothing() {
    let empty = MeasuredSketchAccuracy::default();
    for delta in [0.001, 0.01, 1.0] {
        assert_eq!(
            empty.measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), delta),
            None
        );
    }
}

/// Lookup monotonicity (I10): a larger δ never gets a larger measured
/// error than a smaller one.
#[test]
fn lookup_error_does_not_grow_with_delta() {
    let table = table();
    let deltas = [0.003, 0.005, 0.01, 0.02, 0.05, 0.5, 1.0];
    let errors: Vec<f64> = deltas
        .iter()
        .map(|&delta| {
            parts(
                &table
                    .measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), delta)
                    .unwrap(),
            )
            .0
        })
        .collect();
    assert!(
        errors.windows(2).all(|pair| pair[1] <= pair[0]),
        "{errors:?}"
    );
}

/// A deterministic sketch keeps its deterministic guarantee: a measured
/// (probabilistic) entry for DDSketch α = 0.01 is not returned.
// provisional answer 8
#[test]
fn measured_entry_never_replaces_a_deterministic_guarantee() {
    let table = MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            sketch: SketchParams::DDSketch { alpha: 0.01 },
            statistic: p99(),
            accuracy_metric: AccuracyMetric::Relative,
            trials: 1_000,
            error_at: vec![point(0.01, 0.004)],
        }],
    };
    let guarantee = table.measured_guarantee(
        &FieldDataType::Sketch(SketchAlgorithm::DDSketch),
        &SketchParams::DDSketch { alpha: 0.01 },
        &p99(),
        0.01,
    );
    assert_eq!(guarantee, None);
}

/// Each invalid table is rejected.
#[test]
fn table_validation() {
    assert!(table().validate().is_ok());
    let with = |error_at: Vec<ErrorAtFailureProbability>| MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            error_at,
            ..kll_547_entry()
        }],
    };
    let rejected = [
        // δ = 0.002 < 3 / 1,000 trials.
        ("delta below 3/trials", with(vec![point(0.002, 0.004)])),
        // The error grows with δ.
        (
            "error increasing in delta",
            with(vec![point(0.01, 0.002), point(0.05, 0.003)]),
        ),
        // δ not ascending.
        (
            "unsorted deltas",
            with(vec![point(0.05, 0.0016), point(0.01, 0.0024)]),
        ),
        ("delta zero", with(vec![point(0.0, 0.01)])),
        ("delta above one", with(vec![point(1.5, 0.01)])),
        ("negative error", with(vec![point(0.01, -1.0)])),
        ("NaN error", with(vec![point(0.01, f64::NAN)])),
        (
            "duplicate entry",
            MeasuredSketchAccuracy {
                entries: vec![kll_547_entry(), kll_547_entry()],
            },
        ),
    ];
    for (name, table) in rejected {
        assert!(table.validate().is_err(), "{name} must be rejected");
    }
}

/// The table's JSON is the issue's example format, and round-trips.
#[test]
fn accuracy_json_round_trips() {
    let text = r#"{"entries": [{"sketch": {"Kll": {"k": 800}}, "statistic": {"Quantile": {"q": 0.99}},
        "accuracy_metric": "rank", "trials": 1000, "error_at": [
        {"failure_probability": 0.01, "error": 0.00176},
        {"failure_probability": 0.05, "error": 0.00121}]}]}"#;
    let table: MeasuredSketchAccuracy = serde_json::from_str(text).unwrap();
    assert_eq!(table.entries[0].sketch, SketchParams::Kll { k: 800 });
    assert_eq!(table.entries[0].error_at[1], point(0.05, 0.00121));
    let written = serde_json::to_string(&table).unwrap();
    assert_eq!(
        serde_json::from_str::<MeasuredSketchAccuracy>(&written).unwrap(),
        table
    );
}
