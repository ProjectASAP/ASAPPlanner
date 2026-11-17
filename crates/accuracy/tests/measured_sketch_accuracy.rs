//! The measured accuracy table as Pass 2's [`AccuracyEvidence`]: a request
//! at failure probability δ takes the largest recorded δ' ≤ δ of an entry
//! with exactly its parameters, statistic and metric, and nothing else.
//! Every expected value is read off the fixture by hand.

use asap_accuracy::{
    theoretical_guarantee, AccuracyEvidence, ErrorAtFailureProbability, MeasuredAccuracyEntry,
    MeasuredSketchAccuracy, MeasuredSketchAccuracyError,
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
    let guarantee = theoretical_guarantee(
        &kll(),
        &SketchParams::Kll { k: 547 },
        &p99(),
        0.002,
        None,
        None,
    )
    .unwrap();
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
    // An entry in another metric than the readout's own (Rank for KLL) is
    // never returned.
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
/// (probabilistic) entry for DDSketch α = 0.01 is not returned. The rule
/// is general: the table is never read for a θ whose theoretical guarantee
/// has failure probability zero; DDSketch's relative α is the one such
/// case today.
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

/// Whether a validation error is the expected one.
type IsReason = fn(&MeasuredSketchAccuracyError) -> bool;

/// Each invalid table is rejected, for its own reason: the error variant
/// is asserted, so a table refused for the wrong reason fails the test.
#[test]
fn table_validation() {
    use MeasuredSketchAccuracyError as E;
    assert_eq!(table().validate(), Ok(()));
    let with = |error_at: Vec<ErrorAtFailureProbability>| MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            error_at,
            ..kll_547_entry()
        }],
    };
    let rejected: [(&str, MeasuredSketchAccuracy, IsReason); 9] = [
        // δ = 0.002 < 3 / 1,000 trials.
        (
            "delta below 3/trials",
            with(vec![point(0.002, 0.004)]),
            |e| {
                *e == E::FailureProbabilityOutOfRange {
                    entry: 0,
                    failure_probability: 0.002,
                }
            },
        ),
        // An entry with no points answers no request.
        ("no points", with(vec![]), |e| {
            *e == E::NoPoints { entry: 0 }
        }),
        // The error grows with δ.
        (
            "error increasing in delta",
            with(vec![point(0.01, 0.002), point(0.05, 0.003)]),
            |e| *e == E::NotMonotone { entry: 0 },
        ),
        // δ not ascending.
        (
            "unsorted deltas",
            with(vec![point(0.05, 0.0016), point(0.01, 0.0024)]),
            |e| *e == E::NotMonotone { entry: 0 },
        ),
        ("delta zero", with(vec![point(0.0, 0.01)]), |e| {
            *e == E::FailureProbabilityOutOfRange {
                entry: 0,
                failure_probability: 0.0,
            }
        }),
        ("delta above one", with(vec![point(1.5, 0.01)]), |e| {
            *e == E::FailureProbabilityOutOfRange {
                entry: 0,
                failure_probability: 1.5,
            }
        }),
        ("negative error", with(vec![point(0.01, -1.0)]), |e| {
            *e == E::InvalidError {
                entry: 0,
                error: -1.0,
            }
        }),
        // NaN != NaN, so match the variant and check the error is NaN.
        (
            "NaN error",
            with(vec![point(0.01, f64::NAN)]),
            |e| matches!(e, E::InvalidError { entry: 0, error } if error.is_nan()),
        ),
        (
            "duplicate entry",
            MeasuredSketchAccuracy {
                entries: vec![kll_547_entry(), kll_547_entry()],
            },
            |e| {
                *e == E::DuplicateEntry {
                    first: 0,
                    second: 1,
                }
            },
        ),
    ];
    for (name, table, expected) in rejected {
        let error = table.validate().expect_err(name);
        assert!(expected(&error), "{name}: wrong reason {error:?}");
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

/// A request at failure probability 0, below 0, or NaN has no recorded
/// δ′ ≤ δ: nothing is returned, and nothing panics.
#[test]
fn a_request_at_zero_or_nan_failure_probability_gets_nothing() {
    let table = table();
    for delta in [0.0, -0.1, f64::NAN] {
        assert_eq!(
            table.measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), delta),
            None,
            "δ = {delta}"
        );
    }
}

/// The returned guarantee states the recorded δ′ (0.01), not the requested
/// δ (0.02), so Pass 2's check that a guarantee holds at the requested
/// failure probability (0.01 ≤ 0.02) holds by construction.
#[test]
fn the_returned_guarantee_states_the_recorded_failure_probability() {
    let guarantee = table()
        .measured_guarantee(&kll(), &SketchParams::Kll { k: 547 }, &p99(), 0.02)
        .unwrap();
    assert_eq!(guarantee.spec.failure_probability.evaluate(), Some(0.01));
    assert_eq!(guarantee.spec.accuracy_metric, AccuracyMetric::Rank);
}

/// HLL's theoretical guarantee (relative standard error 1.04 / √m) has an
/// unknown failure probability, so Pass 2 prunes an HLL readout under a
/// target. A measured entry supplies one: HLL precision 13 read for the
/// distinct count, relative error 0.02 at δ = 0.01 over 1,000 trials, gives
/// the guarantee (0.02, 0.01, Relative). Measured evidence comes first, and
/// the deterministic rule does not apply (the theoretical failure
/// probability is unknown, not zero). The recorded error is the worst over
/// the merge counts the calibration measured, so the guarantee also holds
/// for a state merged from that many windows.
#[test]
fn a_measured_entry_supplies_a_guarantee_where_theory_states_no_failure_probability() {
    let hll = FieldDataType::Sketch(SketchAlgorithm::Hll);
    let parameters = SketchParams::Hll { precision: 13 };
    let table = MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            sketch: parameters.clone(),
            statistic: SketchStatistic::Cardinality,
            accuracy_metric: AccuracyMetric::Relative,
            trials: 1_000,
            error_at: vec![point(0.01, 0.02)],
        }],
    };
    assert_eq!(table.validate(), Ok(()));
    let guarantee = table
        .measured_guarantee(&hll, &parameters, &SketchStatistic::Cardinality, 0.01)
        .unwrap();
    assert_eq!(parts(&guarantee), (0.02, 0.01, AccuracyMetric::Relative));
}

/// A readout with no metric of its own (UnivMon's distinct count has no
/// listed guarantee) never gets a measured one: there is no readout metric
/// for the entry's to match.
#[test]
fn an_entry_for_a_readout_with_no_metric_is_never_returned() {
    let univmon = FieldDataType::Sketch(SketchAlgorithm::UnivMon);
    let parameters = SketchParams::UnivMon {
        heap_size: 32,
        sketch_rows: 5,
        sketch_cols: 1024,
        layers: 8,
    };
    let table = MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            sketch: parameters.clone(),
            statistic: SketchStatistic::Cardinality,
            accuracy_metric: AccuracyMetric::Relative,
            trials: 1_000,
            error_at: vec![point(0.01, 0.05)],
        }],
    };
    assert_eq!(
        table.measured_guarantee(&univmon, &parameters, &SketchStatistic::Cardinality, 0.01),
        None
    );
}
