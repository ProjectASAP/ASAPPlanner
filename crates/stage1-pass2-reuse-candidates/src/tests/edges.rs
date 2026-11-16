//! Steps 4 to 6: the logical edges Pass 2 assembles and keeps ($\alpha^{\log}$
//! and the metric rule themselves are tested in `asap_accuracy`).

use asap_accuracy::{ErrorAtFailureProbability, MeasuredAccuracyEntry, MeasuredSketchAccuracy};
use asap_types::accuracy::{AccuracyMetric, AccuracyScope, AccuracyTarget};
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::{ASAPOp, Operator};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, RequiredAccuracy, RootPruneReason,
};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::workload::DataWorkload;

use crate::test_fixtures::*;
use asap_types::test_support::memo_builders::*;
use asap_types::test_support::memo_fixtures::arithmetic;

/// Without accuracy targets (accuracy not enforced) readouts get no
/// requirement and no metric rule applies: every edge is admissible.
#[test]
fn without_targets_every_edge_is_admissible() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, _, p99) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.99, 300);
    let p50 = estimate(
        &mut memo,
        MemoGroupId(2),
        SketchStatistic::Quantile { q: 0.5 },
    );
    let ratio = group(&mut memo, arithmetic(ArithmeticOpKind::Div, p99, p50));
    query(&mut memo, ratio, None);
    let logical = pass2(memo, &[entry(60_000)]);
    assert!(logical.pruned.is_empty());
    assert!(logical
        .edges
        .iter()
        .all(|edge| edge.satisfies_logical_requirement));
}

/// Every candidate of an input group is an edge of each consumer input,
/// except that a candidate carrying a θ without building (a merge, a
/// `TimeRange` over a chain) reads only producers with its θ.
#[test]
fn every_candidate_of_an_input_group_is_an_edge_of_matching_parameters() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, state, readout) = quantile_over_time(&mut memo, SketchAlgorithm::DDSketch, 0.9, 300);
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(300_000)]);
    let listed = parameter_list(SketchAlgorithm::DDSketch).len();
    // The builds and one merge per θ over the 300 s chain.
    assert_eq!(
        edges_into(&logical, own_candidate(readout), 0).len(),
        2 * listed
    );
    let memo = &logical.memo;
    for (index_in_group, merge) in candidates_of(memo, state).iter().enumerate() {
        if !matches!(
            merge.expression.operator,
            Operator::ASAP(ASAPOp::SummaryMerge { .. })
        ) {
            continue;
        }
        let consumer = CandidateForMemoGroupId {
            group: state,
            index_in_group,
        };
        let producers = edges_into(&logical, consumer, 0);
        assert_eq!(producers.len(), 1);
        assert_eq!(
            memo.candidate(producers[0].0)
                .logical_properties
                .parameters(),
            merge.logical_properties.parameters()
        );
    }
}

/// A merge carries the θ of the states it reads.
/// P = `quantile_over_time(0.9, m[5m])` at rank 0.005 keeps KLL builds
/// from k = 547; Q = `quantile_over_time(0.9, m[10m])` at rank 0.01 from
/// k = 269. Both read one 60 s chain, which has the builds of both, so P's
/// state gets `SummaryMerge[k=269]` and `SummaryMerge[k=547]`: the second
/// serves P's readout, the first is pruned with `BelowRequiredAccuracy`.
#[test]
fn a_merge_is_judged_by_the_parameters_it_carries() {
    let mut memo = Memo::new(&DataWorkload::default());
    let (_, p_state, p_readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 300);
    let (_, _, q_readout) = quantile_over_time(&mut memo, SketchAlgorithm::Kll, 0.9, 600);
    query(&mut memo, p_readout, Some(AccuracyTarget::max_error(0.005)));
    query(&mut memo, q_readout, Some(AccuracyTarget::max_error(0.01)));
    let logical = pass2(memo, &[entry(60_000), entry(60_000)]);
    let memo = &logical.memo;
    let merge_with_k = |k: u32| -> CandidateForMemoGroupId {
        let index_in_group = candidates_of(memo, p_state)
            .iter()
            .position(|c| {
                matches!(
                    c.expression.operator,
                    Operator::ASAP(ASAPOp::SummaryMerge { .. })
                ) && c.logical_properties.parameters() == Some(&SketchParams::Kll { k })
            })
            .unwrap_or_else(|| panic!("SummaryMerge[k={k}] in P's state group"));
        CandidateForMemoGroupId {
            group: p_state,
            index_in_group,
        }
    };
    let into_p = edges_into(&logical, own_candidate(p_readout), 0);
    assert!(into_p.contains(&(merge_with_k(547), true)), "{into_p:?}");
    let pruned = pruned_entry(&logical, merge_with_k(269)).expect("SummaryMerge[k=269] is pruned");
    assert!(!pruned.causes.is_empty());
    assert!(pruned.causes.iter().all(|cause| matches!(
        cause.root,
        RootPruneReason::BelowRequiredAccuracy {
            required: RequiredAccuracy {
                metric: AccuracyMetric::Rank,
                max_error,
                ..
            },
            ..
        } if max_error == 0.005
    )));
}

/// A measured accuracy table with one entry: `sketch` read for the 0.99
/// quantile in `accuracy_metric`, over 1,000 trials, with `error` at
/// failure probability 0.01.
fn measured_p99(
    sketch: SketchParams,
    accuracy_metric: AccuracyMetric,
    error: f64,
) -> MeasuredSketchAccuracy {
    MeasuredSketchAccuracy {
        entries: vec![MeasuredAccuracyEntry {
            sketch,
            statistic: SketchStatistic::Quantile { q: 0.99 },
            accuracy_metric,
            trials: 1_000,
            error_at: vec![ErrorAtFailureProbability {
                failure_probability: 0.01,
                error,
            }],
        }],
    }
}

/// `quantile_over_time(0.99, m[5m])` over one series of `m` (so δ is not
/// split across readout values) whose values are non-negative (so
/// DDSketch's sign condition holds), with a per-value `metric` target of
/// `max_error` at δ = 0.01: the memo, its state group and readout group.
fn p99_with_per_value_target(
    algorithm: SketchAlgorithm,
    metric: AccuracyMetric,
    max_error: f64,
) -> (Memo, MemoGroupId, MemoGroupId) {
    let mut memo = Memo::new(&with_non_negative_values("m").with_series_count(1));
    let (_, state, readout) = quantile_over_time(&mut memo, algorithm, 0.99, 300);
    query(
        &mut memo,
        readout,
        Some(
            AccuracyTarget::max_error(max_error)
                .failure_probability(0.01)
                .accuracy_metric(metric)
                .accuracy_scope(AccuracyScope::PerValue),
        ),
    );
    (memo, state, readout)
}

/// The build candidate of `state` with parameters `parameters`.
fn build_with(
    logical: &crate::LogicalASAPMemoCandidates,
    state: MemoGroupId,
    parameters: &SketchParams,
) -> CandidateForMemoGroupId {
    let index_in_group = candidates_of(&logical.memo, state)
        .iter()
        .position(|c| {
            matches!(
                c.expression.operator,
                Operator::ASAP(ASAPOp::SummaryAgg { .. })
            ) && c.logical_properties.parameters() == Some(parameters)
        })
        .unwrap_or_else(|| panic!("a build with {parameters:?} in the state group"));
    CandidateForMemoGroupId {
        group: state,
        index_in_group,
    }
}

/// Whether `candidate` is pruned, every cause `BelowRequiredAccuracy`.
fn pruned_below_required_accuracy(
    logical: &crate::LogicalASAPMemoCandidates,
    candidate: CandidateForMemoGroupId,
) -> bool {
    pruned_entry(logical, candidate).is_some_and(|pruned| {
        !pruned.causes.is_empty()
            && pruned
                .causes
                .iter()
                .all(|cause| matches!(cause.root, RootPruneReason::BelowRequiredAccuracy { .. }))
    })
}

/// Measured accuracy keeps a smaller KLL that theory prunes. Rank target
/// 0.0025 per value at δ = 0.01: in theory KLL k = 547 has rank error
/// 2.296 / 547^0.9723 = 0.004998 > 0.0025 and is pruned, k = 1,404 has
/// 0.00200 and is kept. A table measuring k = 547 at rank error 0.0024
/// at δ′ = 0.01 (≤ 0.0025) keeps the k = 547 edge.
#[test]
fn measured_accuracy_keeps_a_smaller_kll_that_theory_prunes() {
    let k547 = SketchParams::Kll { k: 547 };
    let k1404 = SketchParams::Kll { k: 1404 };

    let (memo, state, readout) =
        p99_with_per_value_target(SketchAlgorithm::Kll, AccuracyMetric::Rank, 0.0025);
    let theory = pass2(memo, &[entry(60_000)]);
    assert!(
        pruned_below_required_accuracy(&theory, build_with(&theory, state, &k547)),
        "without evidence, k = 547 is below the required accuracy"
    );
    let into_readout = edges_into(&theory, own_candidate(readout), 0);
    assert!(
        into_readout.contains(&(build_with(&theory, state, &k1404), true)),
        "{into_readout:?}"
    );

    let (memo, state, readout) =
        p99_with_per_value_target(SketchAlgorithm::Kll, AccuracyMetric::Rank, 0.0025);
    let measured = measured_p99(k547.clone(), AccuracyMetric::Rank, 0.0024);
    let with_table = pass2_with_evidence(memo, &[entry(60_000)], &measured);
    let build_547 = build_with(&with_table, state, &k547);
    assert!(pruned_entry(&with_table, build_547).is_none());
    let into_readout = edges_into(&with_table, own_candidate(readout), 0);
    assert!(
        into_readout.contains(&(build_547, true)),
        "{into_readout:?}"
    );
}

/// Measured accuracy never rescues a DDSketch α: its relative guarantee is
/// deterministic (failure probability zero), so the table is not read. At
/// a per-value relative target of 0.005, α = 0.01 is pruned with and
/// without a table claiming relative error 0.004 at δ = 0.01; α = 0.005
/// is kept in both.
#[test]
fn measured_accuracy_does_not_rescue_a_ddsketch_alpha() {
    let alpha_01 = SketchParams::DDSketch { alpha: 0.01 };
    let alpha_005 = SketchParams::DDSketch { alpha: 0.005 };
    let measured = measured_p99(alpha_01.clone(), AccuracyMetric::Relative, 0.004);
    let without = |memo| pass2(memo, &[entry(60_000)]);
    let with = |memo| pass2_with_evidence(memo, &[entry(60_000)], &measured);
    for (name, run) in [
        ("without the table", &without as &dyn Fn(Memo) -> _),
        ("with the table", &with),
    ] {
        let (memo, state, readout) =
            p99_with_per_value_target(SketchAlgorithm::DDSketch, AccuracyMetric::Relative, 0.005);
        let logical = run(memo);
        assert!(
            pruned_below_required_accuracy(&logical, build_with(&logical, state, &alpha_01)),
            "{name}: α = 0.01 is below the required accuracy"
        );
        let into_readout = edges_into(&logical, own_candidate(readout), 0);
        assert!(
            into_readout.contains(&(build_with(&logical, state, &alpha_005), true)),
            "{name}: {into_readout:?}"
        );
    }
}
