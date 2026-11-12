//! Step 6: the logical edges Pass 2 assembles and keeps ($\alpha^{\log}$
//! and the metric rule themselves are tested in `asap_accuracy`).

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
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
