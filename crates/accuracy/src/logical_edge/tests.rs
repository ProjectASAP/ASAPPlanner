use super::*;
use crate::NoMeasuredGuarantees;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::Reduction;
use asap_types::ir::scalar::ColumnRef;
use asap_types::ir::schema::FieldDataType;
use asap_types::stages::{CandidateForMemoGroup, OriginOfCandidateForMemoGroup};
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

/// The DDSketch relative guarantee and the sign of the values it is
/// built over (sign test plan T5.3 and T4 b12).
mod ddsketch_sign_gate {
    use super::*;
    use asap_types::ir::scalar::ScalarExpr;
    use asap_types::ir::{NonASAPOp, ProjectItem};
    use asap_types::stages::{LogicalRequirements, MemoGroupId, RequiredAccuracy, ValueSign};
    use asap_types::test_support::memo_builders::{
        estimate, group, own_candidate, scan_group, sketch_state,
    };
    use asap_types::test_support::memo_fixtures::range;
    use asap_types::workload::SourceFacts;
    use std::collections::BTreeMap;

    /// A relative error of 5%, as `AccuracyTarget::max_error(0.05)` in
    /// relative error with no failure probability set (1).
    const RELATIVE_5_PERCENT: RequiredAccuracy = RequiredAccuracy {
        metric: AccuracyMetric::Relative,
        max_error: 0.05,
        failure_probability: 1.0,
    };

    /// Sources `pos` (`value` declared non-negative), `neg` (declared
    /// non-positive) and `mixed` (nothing declared).
    fn signed_sources() -> DataWorkload {
        let declared = |sign| SourceFacts {
            column_signs: BTreeMap::from([("value".to_string(), sign)]),
            ..SourceFacts::default()
        };
        DataWorkload {
            sources: BTreeMap::from([
                ("pos".to_string(), declared(ValueSign::NonNegative)),
                ("neg".to_string(), declared(ValueSign::NonPositive)),
                ("mixed".to_string(), SourceFacts::default()),
            ]),
            ..DataWorkload::default()
        }
    }

    /// `abs(value)` of `child`: every other column copied.
    fn abs_of_value(memo: &mut Memo, child: MemoGroupId) -> MemoGroupId {
        let schema = &memo.groups()[child.0 as usize].group_properties.schema;
        let value = asap_types::ir::scalar::resolve_column_ref(&ColumnRef::SampleValue, schema)
            .expect("a sample value");
        let cols = schema
            .fields
            .iter()
            .enumerate()
            .map(|(column, field)| ProjectItem {
                alias: Some(field.name.clone()),
                expr: if column == value {
                    ScalarExpr::FunctionCall {
                        name: "promql_abs".into(),
                        args: vec![ScalarExpr::Column(column)],
                    }
                } else {
                    ScalarExpr::Column(column)
                },
            })
            .collect();
        group(memo, Operator::NonASAP(NonASAPOp::Project { cols, child }))
    }

    /// The outcome of the edge from each DDSketch build of
    /// `quantile_over_time(q, metric[5m])`'s state whose α is within 5%
    /// into its readout, the readout required to be within 5% relative
    /// error (as Pass 2 writes it), under a query with that target whose
    /// root is the readout, or `abs` of it when `abs_root`.
    fn ddsketch_edges_within_five_percent(
        metric: &str,
        q: f64,
        abs_root: bool,
    ) -> Vec<(f64, Result<(), RootPruneReason>)> {
        let mut memo = Memo::new(&signed_sources());
        let input = scan_group(&mut memo, metric, &["job"], vec![]);
        let window = group(&mut memo, range(300_000, input));
        let state = sketch_state(
            &mut memo,
            window,
            SketchAlgorithm::DDSketch,
            Reduction::PerEntity,
        );
        let readout = estimate(&mut memo, state, SketchStatistic::Quantile { q });
        let root = match abs_root {
            true => abs_of_value(&mut memo, readout),
            false => readout,
        };
        query(
            &mut memo,
            root,
            Some(AccuracyTarget::max_error(0.05).accuracy_metric(AccuracyMetric::Relative)),
        );
        memo.set_candidate_input_requirements(
            own_candidate(readout),
            vec![LogicalRequirements {
                required_accuracy: Some(RELATIVE_5_PERCENT),
            }],
        );
        let error_metrics =
            ErrorMetricsOfMemo::of(&memo, &memo.topological_order().expect("acyclic"));
        let edges: Vec<_> = (0..memo.groups()[state.0 as usize].candidates.len())
            .filter_map(|index_in_group| {
                let build = CandidateForMemoGroupId {
                    group: state,
                    index_in_group,
                };
                let Some(SketchParams::DDSketch { alpha }) =
                    memo.candidate(build).logical_properties.parameters()
                else {
                    panic!("a DDSketch build");
                };
                (*alpha <= 0.05).then(|| {
                    (
                        *alpha,
                        error_metrics.edge_meets_logical_requirements(
                            &memo,
                            build,
                            own_candidate(readout),
                            0,
                            &NoMeasuredGuarantees,
                        ),
                    )
                })
            })
            .collect();
        assert!(!edges.is_empty(), "a listed α within 5%");
        edges
    }

    /// DDSketch's relative error α holds for values of one sign: it maps
    /// |v| to a log-spaced bucket in a positive or a negative store. Over
    /// values of both signs a quantile can fall between the stores, where
    /// the relative bound does not hold: of 1,000 values of -1 and
    /// 1,000 of +1, the true median may be -1 or +1, an estimate near 0 is
    /// off by ~100%. So the relative guarantee needs the state column (the
    /// values the sketch is updated with) of one sign: granted over `pos`
    /// (N) and `neg` (P) for every α within 5%, refused over `mixed` (U).
    /// The gate reads the state group's `state_column_sign` at the
    /// logical edge. Today the `mixed` rows are granted.
    #[test]
    fn ddsketch_relative_guarantee_needs_a_one_signed_state_column() {
        // provisional answer Q7: the gate is at the logical edge.
        for (metric, granted) in [("pos", true), ("neg", true), ("mixed", false)] {
            for (alpha, edge) in ddsketch_edges_within_five_percent(metric, 0.9, false) {
                assert_eq!(edge.is_ok(), granted, "{metric}, α = {alpha}: {edge:?}");
            }
        }
    }

    /// `abs(quantile_over_time(0.3, mixed[5m]))` under a 5% relative
    /// target (B12): `abs` keeps the relative error of the quantile
    /// below it, so the root's target reaches the DDSketch readout, which
    /// may not grant it over `mixed`, whose values may have both signs.
    /// The same query over `pos` and `neg` is granted.
    #[test]
    fn b12_ddsketch_relative_guarantee_needs_one_signed_weights() {
        // provisional answer Q7: the gate is at the logical edge.
        for (metric, granted) in [("pos", true), ("neg", true), ("mixed", false)] {
            for (alpha, edge) in ddsketch_edges_within_five_percent(metric, 0.3, true) {
                assert_eq!(edge.is_ok(), granted, "{metric}, α = {alpha}: {edge:?}");
            }
        }
    }
}
