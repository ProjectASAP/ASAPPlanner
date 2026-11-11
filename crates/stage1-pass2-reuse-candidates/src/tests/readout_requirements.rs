//! Step 3: readout requirements.

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::QueryRoot;
use asap_types::stages::{
    Memo, MemoGroupId, MemoQuery, PruneCause, RequiredAccuracy, RootPruneReason,
};
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;

/// A single quantile query with rank error 0.01: its one readout gets that
/// requirement (λ = 1), and only the KLL builds whose rank error at δ = 1
/// is within 0.01 satisfy it.
#[test]
fn a_single_readout_requires_the_query_target() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    query(&mut memo, readout, Some(AccuracyTarget::max_error(0.01)));
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(
        requirements_of(&logical.memo, readout),
        vec![Some(RequiredAccuracy {
            metric: AccuracyMetric::Rank,
            max_error: 0.01,
            failure_probability: 1.0,
        })]
    );
    let admitted: Vec<u32> = edges_into(&logical, own_candidate(readout), 0)
        .into_iter()
        .filter(|&(_, satisfies)| satisfies)
        .filter_map(|(producer, _)| {
            match logical
                .memo
                .candidate(producer)
                .logical_properties
                .parameters()
            {
                Some(SketchParams::Kll { k }) => Some(*k),
                _ => None,
            }
        })
        .collect();
    assert!(!admitted.is_empty());
    assert!(admitted.iter().all(|&k| k >= 269), "{admitted:?}");
    assert!(logical
        .pruned
        .iter()
        .filter(|p| p.candidate.group == state)
        .all(|p| p
            .causes
            .iter()
            .all(|cause| matches!(cause.root, RootPruneReason::BelowRequiredAccuracy { .. }))));
}

/// A whole-result failure probability is split over the values one
/// evaluation returns: δ = 0.01 over 100 series is 1e-4 per value.
#[test]
fn a_whole_result_failure_probability_is_split_per_value() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        number_of_values_per_query_evaluation: Some(100),
    });
    let logical = pass2(memo, &[entry(60_000)]);
    let [Some(required)] = requirements_of(&logical.memo, readout)[..] else {
        panic!("one requirement");
    };
    assert!((required.failure_probability - 1e-4).abs() < 1e-12);
}

/// A whole-result failure probability with the
/// number of values per evaluation unknown gives no per-value one, so the
/// readouts the query reaches are pruned with `ValuesPerEvaluationUnknown`
/// instead of requiring δ = 0.
#[test]
fn a_whole_result_failure_probability_without_values_per_evaluation_prunes_the_readout() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    let query = memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        number_of_values_per_query_evaluation: None,
    });
    let logical = pass2(memo, &[entry(60_000)]);
    let pruned = pruned_entry(&logical, own_candidate(readout)).expect("the readout is pruned");
    assert!(!pruned.causes.is_empty());
    for cause in &pruned.causes {
        assert!(
            matches!(
                cause.root,
                RootPruneReason::ValuesPerEvaluationUnknown { consumer, query: q }
                    if consumer == own_candidate(readout) && q == query
            ),
            "{cause:?}"
        );
    }
}

/// A readout another query gives a level is not pruned for a query whose
/// whole-result failure probability has no per-value one: Q0 (δ = 0.01,
/// values per evaluation unknown) and Q1 (no δ) read one KLL readout, and
/// the readout keeps Q1's level, live; Stage 3 picks a producer per query.
#[test]
fn a_readout_another_query_gives_a_level_survives_unknown_values_per_evaluation() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(readout),
        accuracy_target: Some(AccuracyTarget::max_error(0.01).failure_probability(0.01)),
        number_of_values_per_query_evaluation: None,
    });
    query(&mut memo, readout, Some(AccuracyTarget::max_error(0.01)));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    assert_eq!(
        requirements_of(&logical.memo, readout),
        vec![Some(RequiredAccuracy {
            metric: AccuracyMetric::Rank,
            max_error: 0.01,
            failure_probability: 1.0,
        })]
    );
    let pruned = pruned_entry(&logical, own_candidate(readout));
    assert!(pruned.is_none(), "{pruned:?}");
}

/// A ratio reads no rank error: with a relative target, the KLL readout's
/// edges into the division and from its states fail the metric rule; it
/// gets no requirement.
#[test]
fn a_ratio_reads_no_rank_error() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, p99) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 300);
    let p50 = estimate(
        &mut memo,
        MemoGroupId(2),
        SketchStatistic::Quantile { q: 0.5 },
    );
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p99, p50);
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, ratio, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    assert_eq!(requirements_of(&logical.memo, p99), vec![None]);
    // Its metric, rank, is accepted neither by the ratio nor at its own
    // output: every edge into it is cut.
    let pruned = pruned_entry(&logical, own_candidate(p99)).expect("pruned");
    assert!(!pruned.causes.is_empty());
    assert!(pruned.causes.iter().all(|cause| matches!(
        cause,
        PruneCause {
            root: RootPruneReason::MetricRule {
                producer_metric: AccuracyMetric::Rank,
                ..
            },
            ..
        }
    )));
}

/// The snapshot workload
/// `ratio-of-quantiles-1h-mvp`: `quantile_over_time(0.9, data[1h]) /
/// quantile_over_time(0.5, data[1h])` with relative error 0.05 and δ = 0.01
/// over the whole result of 10 000 series is δ = 1e-6 per value, below the
/// grid's 1e-5. The δ levels continue in powers of ten below it, so both
/// readouts find levels whose sum stays within 1e-6 and (K) can hold.
#[test]
fn a_ratio_whose_per_value_failure_probability_is_below_the_grid_splits_it_over_its_readouts() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, p90) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 3600);
    let p50 = estimate(&mut memo, state, SketchStatistic::Quantile { q: 0.5 });
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p90, p50);
    memo.add_query(MemoQuery {
        root: QueryRoot::Operator(ratio),
        accuracy_target: Some(
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        ),
        number_of_values_per_query_evaluation: Some(10_000),
    });
    let logical = pass2(memo, &[entry(60_000)]);
    let smallest_failure_probability = |readout| {
        requirements_of(&logical.memo, readout)
            .into_iter()
            .map(|required| {
                required
                    .expect("every readout has a level")
                    .failure_probability
            })
            .fold(f64::INFINITY, f64::min)
    };
    let (p90_delta, p50_delta) = (
        smallest_failure_probability(p90),
        smallest_failure_probability(p50),
    );
    assert!(
        p90_delta + p50_delta <= 1e-6,
        "{p90_delta} + {p50_delta} > 1e-6"
    );
}
