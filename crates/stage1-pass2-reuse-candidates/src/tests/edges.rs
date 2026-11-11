//! Step 6: the logical edges, $\alpha^{\log}$ and the metric rule, and
//! the evaluation grids.

use asap_types::accuracy::{AccuracyMetric, AccuracyTarget};
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::expr_ir::ArithmeticOpKind;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, Predicate};
use asap_types::stages::{
    CandidateForMemoGroupId, Memo, MemoGroupId, PruneCause, RequiredAccuracy, RootPruneReason,
};
use asap_types::summary_type::parameter_list;
use asap_types::summary_type::{ExactKind, SketchAlgorithm, SketchParams, SketchStatistic};
use asap_types::workload::{
    DataWorkload, QueryRecurrence, QueryWorkloadEntry, RepeatedDemand, RepetitionInterval,
};

use crate::test_fixtures::*;

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
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p99, p50);
    query(&mut memo, ratio, None);
    let logical = pass2(memo, &[entry(60_000)]);
    assert!(logical.pruned.is_empty());
    assert!(logical
        .edges
        .iter()
        .all(|edge| edge.satisfies_logical_requirement));
}

/// A top-k heap smaller than k never serves the readout, whatever accuracy
/// is required: `topk(50)` reads only heaps of at least 50.
#[test]
fn a_heap_smaller_than_k_is_pruned() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "x", &["job"]);
    let window = range(&mut memo, input, 300);
    let state = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::CmsWithHeap,
        Reduction::by(vec![]),
    );
    let readout = estimate(&mut memo, state, SketchStatistic::TopK { k: 50 });
    query(&mut memo, readout, None);
    let logical = pass2(memo, &[entry(60_000)]);
    let mut small = 0;
    for pruned in logical.pruned.iter().filter(|p| p.candidate.group == state) {
        let Some(SketchParams::CmsWithHeap { heap_size, .. }) = logical
            .memo
            .candidate(pruned.candidate)
            .logical_properties
            .parameters()
        else {
            panic!("a heap build");
        };
        assert!(*heap_size < 50);
        assert!(matches!(
            pruned.causes[..],
            [PruneCause {
                root: RootPruneReason::HeapSmallerThanTopK { k: 50, .. },
                ..
            }]
        ));
        small += 1;
    }
    // Heaps of 10 and 20, with each width and depth.
    assert_eq!(small, 2 * 28);
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

/// A group's evaluation grid is the gcd of its reaching queries' grids;
/// `None` when one has no known grid.
#[test]
fn a_groups_evaluation_grid_is_the_gcd_of_its_queries_grids() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let (_, a) = exact_state(&mut memo, window, ExactKind::Sum, Reduction::PerEntity);
    let other = scan(&mut memo, "n", &["job"]);
    query(&mut memo, a, None);
    query(&mut memo, a, None);
    query(&mut memo, other, None);
    let without_phase = QueryWorkloadEntry {
        recurrence: QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
            60_000,
        ))),
        ..entry(60_000)
    };
    let logical = pass2(memo, &[entry(60_000), entry(120_000), without_phase]);
    let grid = |group: MemoGroupId| logical.evaluation_grid_of_group[group.0 as usize];
    assert_eq!(grid(window).map(|g| g.period_ms), Some(60_000));
    assert_eq!(grid(other), None);
}

/// A `Filter` whose predicate reads only label
/// columns keeps or drops whole rows, so a value's error passes through it
/// unchanged. `quantile_over_time(0.99, latency[5m])` behind `job = "api"`,
/// relative error 0.05: the DDSketch readout keeps an admissible edge into
/// the filter. (The contract alone is tested in `asap_types::stages`.)
#[test]
fn a_key_only_filter_keeps_the_ddsketch_readout_admissible() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "latency", &["job"]);
    let window = range(&mut memo, input, 300);
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &[SketchAlgorithm::DDSketch]);
    let job = memo.groups()[p99.result.0 as usize]
        .group_properties
        .schema
        .fields
        .iter()
        .position(|field| field.name == "job")
        .expect("the result keeps the job label");
    let filtered = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Filter {
            pred: Predicate(equals(job, "api")),
            child: p99.result,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, filtered, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    let ddsketch_readout = p99.by_algorithm[0].2;
    let edges = edges_into(&logical, own_candidate(filtered), 0);
    assert!(
        edges.contains(&(ddsketch_readout, true)),
        "edges into the filter: {edges:?}"
    );
}

/// `avg` has an absolute contract only here; a relative bound on a mean
/// needs the values' signs. So
/// `avg(quantile_over_time(0.99, m[5m]))` with a relative target has no
/// admissible edge from a sketch readout into the `avg`: it reads the
/// exact quantile only.
#[test]
fn avg_of_readouts_under_a_relative_target_reads_exact_only() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let algorithms = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
    let p99 = quantile_over_time_readouts(&mut memo, window, 0.99, &algorithms);
    let avg = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: p99.result,
            reduction: Reduction::by(vec![]),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec!["avg".into()],
            filters: vec![None],
            having: None,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative);
    query(&mut memo, avg, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    let edges = edges_into(&logical, own_candidate(avg), 0);
    for (_, _, readout) in &p99.by_algorithm {
        assert!(
            !edges.contains(&(*readout, true)),
            "a sketch readout is admissible into avg: {edges:?}"
        );
    }
    assert!(
        edges.contains(&(own_candidate(p99.result), true)),
        "the exact quantile stays admissible: {edges:?}"
    );
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

/// The metric rule cuts edges, not candidates.
/// `avg by (job)(q0.5 / q0.5 + q0.9)` at absolute error 0.05, each
/// quantile per series with KLL, DDSketch and exact candidates: the `+`
/// carries absolute error only, so the division's inputs accept no metric
/// and the edges from the DDSketch and KLL readouts into it are cut; the
/// exact division stays, read from the exact quantile, and its edge into
/// the `+` holds.
#[test]
fn the_metric_rule_cuts_the_readout_edges_and_keeps_the_exact_division() {
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan(&mut memo, "m", &["job"]);
    let window = range(&mut memo, input, 300);
    let algorithms = [SketchAlgorithm::Kll, SketchAlgorithm::DDSketch];
    let p50 = quantile_over_time_readouts(&mut memo, window, 0.5, &algorithms);
    let p90 = quantile_over_time_readouts(&mut memo, window, 0.9, &algorithms);
    let ratio = arithmetic(&mut memo, ArithmeticOpKind::Div, p50.result, p50.result);
    let sum = arithmetic(&mut memo, ArithmeticOpKind::Add, ratio, p90.result);
    let avg = group(
        &mut memo,
        Operator::NonASAP(NonASAPOp::Aggregate {
            child: sum,
            reduction: Reduction::by(vec![0]),
            measures: vec![AggIntent::Avg { col: None }],
            output_names: vec!["avg".into()],
            filters: vec![None],
            having: None,
        }),
    );
    let target = AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::ABSOLUTE);
    query(&mut memo, avg, Some(target));
    let logical = pass2(memo, &[entry(60_000)]);
    let division = own_candidate(ratio);
    assert!(
        pruned_entry(&logical, division).is_none(),
        "{:?}",
        logical.pruned
    );
    assert!(edges_into(&logical, own_candidate(sum), 0).contains(&(division, true)));
    for input in 0..2 {
        let into_division = edges_into(&logical, division, input);
        assert!(into_division.contains(&(own_candidate(p50.result), true)));
        for (_, _, readout) in &p50.by_algorithm {
            assert!(
                !into_division.contains(&(*readout, true)),
                "{into_division:?}"
            );
        }
    }
}
