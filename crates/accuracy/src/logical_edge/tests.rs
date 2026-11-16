use super::*;
use crate::NoMeasuredGuarantees;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::stages::{
    CandidateForMemoGroup, LogicalRequirements, OriginOfCandidateForMemoGroup, RequiredAccuracy,
};
use asap_types::summary_type::{parameter_list, SketchAlgorithm, SummaryUpdate};
use asap_types::test_support::memo_builders::query;
use asap_types::test_support::memo_fixtures::scan;
use asap_types::workload::DataWorkload;

/// A bound equal to the limit up to floating-point noise meets it; a
/// bound one part in a million above it does not.
#[test]
fn within_limit_admits_only_floating_point_noise() {
    assert!(within_limit(0.01 * (1.0 + 1e-12), 0.01));
    assert!(!within_limit(0.01 * (1.0 + 1e-6), 0.01));
    assert!(within_limit(0.0, 0.0));
}

/// A distinct count read from HLL (relative error, failure probability
/// unknown) or UnivMon (no guarantee for it) under a query with a
/// relative target: the edge fails with `NoGuarantee`, its error would
/// reach the root unbounded; with no target reaching it, it holds.
#[test]
fn a_distinct_count_readout_without_an_evaluable_guarantee_serves_no_query_with_a_target() {
    for algorithm in [SketchAlgorithm::Hll, SketchAlgorithm::UnivMon] {
        let mut memo = Memo::new(&DataWorkload::default());
        let events = memo.find_or_add_group(scan("events", &[], vec![])).unwrap();
        let build = Operator::ASAP(ASAPOp::SummaryAgg {
            child: events,
            summary_type: FieldDataType::Sketch(algorithm),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
        });
        let state = memo.find_or_add_group(build.clone()).unwrap();
        let parameters = parameter_list(algorithm)[0].clone();
        let producer = memo
            .add_candidate(
                state,
                CandidateForMemoGroup::new(
                    build,
                    Some(parameters),
                    OriginOfCandidateForMemoGroup::Pass1,
                    Vec::new(),
                ),
            )
            .unwrap();
        let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
            summary_input: state,
            query: SketchStatistic::Cardinality,
        });
        let distinct = memo.find_or_add_group(readout.clone()).unwrap();
        let consumer = memo
            .add_candidate(
                distinct,
                CandidateForMemoGroup::new(
                    readout,
                    None,
                    OriginOfCandidateForMemoGroup::Pass1,
                    Vec::new(),
                ),
            )
            .unwrap();
        let edge = |memo: &Memo| {
            let order = memo.topological_order().expect("acyclic");
            ErrorMetricsOfMemo::of(memo, &order).edge_meets_logical_requirements(
                memo,
                producer,
                consumer,
                0,
                &NoMeasuredGuarantees,
            )
        };
        assert!(edge(&memo).is_ok(), "{algorithm:?}");
        query(
            &mut memo,
            distinct,
            Some(AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative)),
        );
        assert!(
            matches!(
                edge(&memo),
                Err(RootPruneReason::NoGuarantee { input: 0, .. })
            ),
            "{algorithm:?}: {:?}",
            edge(&memo)
        );
    }
}

/// A top-k heap smaller than k never serves the readout, whatever
/// accuracy is required: the edge from each build into `topk(50)`'s
/// readout fails with `HeapSmallerThanTopK` exactly for the heaps
/// below 50 (sizes 10 and 20, with each width and depth).
#[test]
fn a_heap_smaller_than_k_fails_the_topk_readouts_edge() {
    use asap_types::test_support::memo_builders::{
        estimate, group, own_candidate, scan_group, sketch_state,
    };
    use asap_types::test_support::memo_fixtures::range;
    let mut memo = Memo::new(&DataWorkload::default());
    let input = scan_group(&mut memo, "x", &["job"], vec![]);
    let window = group(&mut memo, range(300_000, input));
    let state = sketch_state(
        &mut memo,
        window,
        SketchAlgorithm::CmsWithHeap,
        Reduction::by(vec![]),
    );
    let readout = estimate(&mut memo, state, SketchStatistic::TopK { k: 50 });
    let error_metrics = ErrorMetricsOfMemo::of(&memo, &memo.topological_order().expect("acyclic"));
    let mut small = 0;
    for index_in_group in 0..memo.groups()[state.0 as usize].candidates.len() {
        let build = CandidateForMemoGroupId {
            group: state,
            index_in_group,
        };
        let Some(SketchParams::CmsWithHeap { heap_size, .. }) =
            memo.candidate(build).logical_properties.parameters()
        else {
            panic!("a heap build");
        };
        let edge = error_metrics.edge_meets_logical_requirements(
            &memo,
            build,
            own_candidate(readout),
            0,
            &NoMeasuredGuarantees,
        );
        if *heap_size < 50 {
            assert!(
                matches!(
                    edge,
                    Err(RootPruneReason::HeapSmallerThanTopK { k: 50, .. })
                ),
                "{edge:?}"
            );
            small += 1;
        } else {
            assert!(edge.is_ok(), "{edge:?}");
        }
    }
    assert_eq!(small, 2 * 28);
}

/// The edge from the build of `algorithm` at `parameters` over `Scan events` (whose
/// facts are `facts`) into a readout of `statistic` required at
/// `required`, which a query with `target` reads.
fn readout_edge(
    facts: asap_types::workload::SourceFacts,
    algorithm: SketchAlgorithm,
    parameters: SketchParams,
    statistic: SketchStatistic,
    required: RequiredAccuracy,
    target: AccuracyTarget,
) -> Result<(), RootPruneReason> {
    let data = DataWorkload {
        sources: [("events".to_string(), facts)].into(),
        ..DataWorkload::default()
    };
    let mut memo = Memo::new(&data);
    let events = memo.find_or_add_group(scan("events", &[], vec![])).unwrap();
    let build = Operator::ASAP(ASAPOp::SummaryAgg {
        child: events,
        summary_type: FieldDataType::Sketch(algorithm),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::PerEntity,
        filter: None,
    });
    let state = memo.find_or_add_group(build.clone()).unwrap();
    let producer = memo
        .add_candidate(
            state,
            CandidateForMemoGroup::new(
                build,
                Some(parameters),
                OriginOfCandidateForMemoGroup::Pass1,
                Vec::new(),
            ),
        )
        .unwrap();
    let readout = Operator::ASAP(ASAPOp::SummaryEstimate {
        summary_input: state,
        query: statistic,
    });
    let group = memo.find_or_add_group(readout.clone()).unwrap();
    let mut candidate = CandidateForMemoGroup::new(
        readout,
        None,
        OriginOfCandidateForMemoGroup::Pass1,
        Vec::new(),
    );
    candidate.input_requirements = vec![LogicalRequirements {
        required_accuracy: Some(required),
    }];
    let consumer = memo.add_candidate(group, candidate).unwrap();
    query(&mut memo, group, Some(target));
    let order = memo.topological_order().expect("acyclic");
    ErrorMetricsOfMemo::of(&memo, &order).edge_meets_logical_requirements(
        &memo,
        producer,
        consumer,
        0,
        &NoMeasuredGuarantees,
    )
}

/// `COUNT(DISTINCT user_id)` within relative 0.05 at δ = 0.01 from classic
/// HLL at precision 18: the edge holds over a source declaring at most
/// 9,636 distinct items per evaluation, and fails with `NoGuarantee` over
/// 1,000,000 (outside the contract) or without a bound.
#[test]
fn a_distinct_count_readout_reads_the_scanned_sources_distinct_bound() {
    for (bound, holds) in [(Some(9_636), true), (Some(1_000_000), false), (None, false)] {
        let facts = asap_types::workload::SourceFacts {
            max_distinct_per_evaluation: bound,
            ..Default::default()
        };
        let edge = readout_edge(
            facts,
            SketchAlgorithm::Hll,
            SketchParams::Hll { precision: 18 },
            SketchStatistic::Cardinality,
            RequiredAccuracy {
                metric: AccuracyMetric::Relative,
                max_error: 0.05,
                failure_probability: 0.01,
            },
            AccuracyTarget::max_error(0.05)
                .failure_probability(0.01)
                .accuracy_metric(AccuracyMetric::Relative),
        );
        match holds {
            true => assert!(edge.is_ok(), "{bound:?}: {edge:?}"),
            false => assert!(
                matches!(edge, Err(RootPruneReason::NoGuarantee { input: 0, .. })),
                "{bound:?}: {edge:?}"
            ),
        }
    }
}

/// DDSketch's relative error on a quantile holds for values of one sign:
/// `quantile_over_time(0.5, events[..])` at relative 0.01 from α = 0.01
/// holds when `events`' value is declared non-negative, and fails with
/// `NoGuarantee` when its sign is unknown.
#[test]
fn a_ddsketch_quantile_holds_only_over_values_of_one_sign() {
    for (sign, holds) in [(Some(ValueSign::NonNegative), true), (None, false)] {
        let facts = asap_types::workload::SourceFacts {
            column_signs: sign
                .map(|sign| ("value".to_string(), sign))
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let edge = readout_edge(
            facts,
            SketchAlgorithm::DDSketch,
            SketchParams::DDSketch { alpha: 0.01 },
            SketchStatistic::Quantile { q: 0.5 },
            RequiredAccuracy {
                metric: AccuracyMetric::Relative,
                max_error: 0.01,
                failure_probability: 1.0,
            },
            AccuracyTarget::max_error(0.01).accuracy_metric(AccuracyMetric::Relative),
        );
        match holds {
            true => assert!(edge.is_ok(), "{sign:?}: {edge:?}"),
            false => assert!(
                matches!(edge, Err(RootPruneReason::NoGuarantee { input: 0, .. })),
                "{sign:?}: {edge:?}"
            ),
        }
    }
}
