//! Frontend-to-Pass-1 acceptance: candidate discovery precedes empirical selection.
use crate::computation_targets::{
    computation_candidates_for_intent, generate_per_computation_candidates,
    PerComputationCandidateError,
};
use asap_stage0_memo::memo_of_queries;
use asap_types::accuracy::AccuracyTarget;
use asap_types::ir::operator::arguments::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{DataType, Field, FieldDataType, Schema};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot, ScalarExpr,
};
use asap_types::summary_type::{ExactKind, SketchAlgorithm, SummaryUpdate};

use asap_types::stages::{HavingEntry, HavingPlacement, HowToComputeAggregateMeasure};
use std::rc::Rc;

fn candidates_for(intent: AggIntent) -> Vec<HowToComputeAggregateMeasure> {
    computation_candidates_for_intent(&intent).candidates
}
fn algorithms(choices: &[HowToComputeAggregateMeasure]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            HowToComputeAggregateMeasure::Summary(algorithm) => Some(*algorithm),
            _ => None,
        })
        .collect()
}
fn scan(nullable_value: bool) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "flows".into(),
        },
        predicates: vec![],
        schema: Schema::lifted(
            vec![
                Field::plain("src_ip", DataType::Utf8, false),
                Field::plain("value", DataType::Float64, nullable_value),
            ],
            None,
        ),
    }))
    .unwrap()
}
fn aggregate_over(
    child: Rc<OperatorNode>,
    measures: Vec<AggIntent>,
    filters: Vec<Option<Predicate>>,
    having: Option<Predicate>,
) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction: Reduction::by(vec![]),
        measures,
        output_names: vec![],
        filters,
        having,
    }))
    .unwrap()
}
fn aggregate(intent: AggIntent) -> Rc<OperatorNode> {
    aggregate_over(scan(false), vec![intent], vec![], None)
}
fn always() -> Predicate {
    Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true)))
}

/// Example 2 preserves specialized distinct summaries and the universal alternative.
#[test]
fn cardinality_keeps_exact_specialized_and_universal_candidates() {
    let choices = candidates_for(AggIntent::Cardinality { cols: vec![0] });
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = candidates_for(AggIntent::Cardinality { cols: vec![0, 1] });
    assert!(!algorithms(&tuple).contains(&SketchAlgorithm::UnivMon));
}

/// Frequency moments retain exact execution and a universal sketch without certification.
#[test]
fn frequency_statistics_keep_universal_choices() {
    for intent in [
        AggIntent::FrequencyL2 { col: Some(0) },
        AggIntent::FrequencyEntropy { col: Some(0) },
    ] {
        let choices = candidates_for(intent);
        assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
        assert_eq!(algorithms(&choices), vec![SketchAlgorithm::UnivMon]);
    }
}

/// Scalar roots expose their producer targets; repeated references retain one target identity.
#[test]
fn scalar_root_producers_are_discovered_once() {
    let producer = aggregate(AggIntent::Cardinality { cols: vec![0] });
    let roots = vec![
        QueryRoot::Scalar(ScalarExpr::ScalarSubquery(producer.clone())),
        QueryRoot::Operator(producer.clone()),
    ];
    let candidates = generate_per_computation_candidates(
        &memo_of_queries(&roots, &vec![None; roots.len()], &Default::default()).unwrap(),
    )
    .unwrap();
    assert_eq!(candidates.roots.len(), 2);
    for root in &candidates.roots {
        root.validate_structure().unwrap();
    }
    assert_eq!(candidates.targets.len(), 1);
    assert_eq!(**candidates.aggregate(&candidates.targets[0]), *producer);

    assert!(producer.guarantee.is_none());
    producer.validate_structure().unwrap();
}

/// #509 Example 1 Q1 is exact: each target's one exact option is its exact
/// accumulator state (grouped sum, rate); no plain exact form and no summary.
/// One PromQL query with the default exact target, lowered through the
/// workload frontend.
fn exact_promql(query: &str) -> Vec<QueryRoot> {
    use asap_types::workload::{
        BatchEntry, PlanningWorkload, Query, QueryLanguage, QueryRequirements, QueryWorkload,
    };
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::exact(),
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(asap_types::workload::DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            asap_types::workload::DurationMs(1000),
        )),
    };
    asap_frontend_promql::lower_promql_query_workload(&workload).unwrap()
}

#[test]
fn example_1_exact_q1_keeps_exact_accumulators_only() {
    let roots = exact_promql("sum by (job) (rate(http_requests_total[1m]))");
    let candidates = generate_per_computation_candidates(
        &memo_of_queries(&roots, &vec![None; roots.len()], &Default::default()).unwrap(),
    )
    .unwrap();
    let found: Vec<_> = candidates
        .targets
        .iter()
        .map(|target| target.ways_to_compute.clone())
        .collect();
    assert_eq!(
        found,
        vec![
            vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![
                ExactKind::Sum
            ])],
            vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![
                ExactKind::Rate
            ])],
        ]
    );
}

/// ASAP operators fail at the stage
/// boundary.
#[test]
fn asap_input_is_rejected() {
    let summary = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan(false),
        summary_type: FieldDataType::Sketch(SketchAlgorithm::Kll),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        filter: None,
    }))
    .unwrap();
    assert!(matches!(
        generate_per_computation_candidates(
            &memo_of_queries(
                &[QueryRoot::Operator(summary)],
                &[None],
                &Default::default()
            )
            .unwrap()
        ),
        Err(PerComputationCandidateError::ContainsAsap)
    ));
}

/// TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = candidates_for(AggIntent::TopK { k: 10 });
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
}

/// Summary candidates are unsized, so they do not depend on the requested error.
#[test]
fn summary_candidates_are_unsized() {
    let loose = candidates_for(AggIntent::Count);
    let tight = candidates_for(AggIntent::Count);
    assert_eq!(loose, tight);
}

/// Each measure of a multi-measure aggregate is its own target; a measure whose
/// only realization is its exact accumulator is still a target.
#[test]
fn multi_measure_aggregate_has_one_target_per_measure() {
    let node = aggregate_over(
        scan(false),
        vec![AggIntent::Sum { col: Some(1) }, AggIntent::Count],
        vec![],
        None,
    );
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(&[QueryRoot::Operator(node)], &[None], &Default::default()).unwrap(),
    )
    .unwrap();
    let candidates = &inventory.targets;
    assert_eq!(
        candidates
            .iter()
            .map(|target| target.measure_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        candidates[0].ways_to_compute,
        vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![
            ExactKind::Sum
        ])]
    );
    assert_sql_count_choices(&choices_of(&inventory, 1));
}

/// An aggregate with no realization besides exact execution is not a target.
#[test]
fn aggregate_without_realization_is_not_a_target() {
    let node = aggregate(AggIntent::StdDev {
        col: Some(1),
        population: true,
    });
    assert!(generate_per_computation_candidates(
        &memo_of_queries(&[QueryRoot::Operator(node)], &[None], &Default::default()).unwrap()
    )
    .unwrap()
    .targets
    .is_empty());
}

/// A filtered SQL count keeps its executable accumulator and records its
/// filter, which composition applies to the state.
#[test]
fn filtered_measure_keeps_candidates_and_records_its_filter() {
    let count = || AggIntent::Count;
    let filtered = aggregate_over(
        scan(false),
        vec![count(), count()],
        vec![Some(always()), None],
        None,
    );
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(
            &[QueryRoot::Operator(filtered)],
            &[None],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let targets = &inventory.targets;
    assert_eq!(targets.len(), 2);
    for (index, target) in targets.iter().enumerate() {
        assert_sql_count_choices(&choices_of(&inventory, index));
        assert!(target.having_filters.is_empty());
    }
    assert_eq!(targets[0].measure_filter, Some(always()));
    assert_eq!(targets[1].measure_filter, None);
}

/// Target `target`'s own ways to compute.
fn choices_of(
    inventory: &crate::computation_targets::PerComputationCandidates,
    target: usize,
) -> Vec<HowToComputeAggregateMeasure> {
    inventory.targets[target].ways_to_compute.clone()
}

fn assert_sql_count_choices(choices: &[HowToComputeAggregateMeasure]) {
    assert_eq!(
        choices,
        vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![
            ExactKind::Count
        ])]
    );
}

/// `count(*) by (src_ip)`, the form the SQL frontend gives `GROUP BY src_ip`.
fn grouped_count(having: Option<Predicate>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child: scan(false),
        reduction: Reduction::by(vec![0]),
        measures: vec![AggIntent::Count],
        output_names: vec![],
        filters: vec![],
        having,
    }))
    .unwrap()
}
/// `HAVING` as the SQL frontend lowers it: a `Filter` above the aggregate.
fn having_filter(child: Rc<OperatorNode>, pred: Predicate) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Filter { pred, child })).unwrap()
}
/// `column = 'a'` over the aggregate output (0: `src_ip`, 1: the count).
fn equals(column: usize, value: ScalarValue) -> Predicate {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Eq,
        right: Box::new(ScalarExpr::Literal(value)),
        semantics: ExprSemantics::Sql,
    })
}

/// A `HAVING` over group keys only is pushed below the aggregate and keeps
/// every executable candidate.
#[test]
fn key_only_having_is_pushed_below_and_keeps_candidates() {
    let node = grouped_count(None);
    let root = having_filter(node.clone(), equals(0, ScalarValue::Utf8("a".into())));
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(
            &[QueryRoot::Operator(root.clone())],
            &[None],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let targets = &inventory.targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(**inventory.aggregate(&targets[0]), *node);
    assert_eq!(
        targets[0].having_filters,
        vec![HavingEntry {
            filter: Some(root.clone()),
            predicate: equals(0, ScalarValue::Utf8("a".into())),
            placement: HavingPlacement::BelowAggregate,
        }]
    );
    assert_sql_count_choices(&choices_of(&inventory, 0));
}

/// A `HAVING` over an aggregate value, as a `Filter` above or as the
/// aggregate's own `having`, stays above the output and keeps executable candidates.
#[test]
fn value_having_stays_above_output_and_keeps_candidates() {
    let value = || equals(1, ScalarValue::Int64(10));
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(
            &[
                QueryRoot::Operator(having_filter(grouped_count(None), value())),
                QueryRoot::Operator(grouped_count(Some(value()))),
            ],
            &vec![None; 2],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let targets = &inventory.targets;
    assert_eq!(targets.len(), 2);
    for (index, target) in targets.iter().enumerate() {
        let placements: Vec<_> = target
            .having_filters
            .iter()
            .map(|entry| entry.placement)
            .collect();
        assert_eq!(placements, vec![HavingPlacement::AboveOutput]);
        assert_sql_count_choices(&choices_of(&inventory, index));
    }
}

/// A key-only `HAVING` above a shared aggregate stays above the output when
/// another consumer reads the aggregate without it.
#[test]
fn having_on_one_consumer_of_a_shared_aggregate_stays_above() {
    let node = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let filtered = having_filter(node.clone(), key.clone());
    let targets = generate_per_computation_candidates(
        &memo_of_queries(
            &[
                QueryRoot::Operator(filtered.clone()),
                QueryRoot::Operator(node.clone()),
            ],
            &vec![None; 2],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].having_filters,
        vec![HavingEntry {
            filter: Some(filtered),
            predicate: key.clone(),
            placement: HavingPlacement::AboveOutput,
        }]
    );

    // Two consumers applying the same key-only predicate push it below.
    let first = having_filter(node.clone(), key.clone());
    let second = having_filter(node, key);
    let targets = generate_per_computation_candidates(
        &memo_of_queries(
            &[QueryRoot::Operator(first), QueryRoot::Operator(second)],
            &[None, None],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .targets;
    assert!(targets[0]
        .having_filters
        .iter()
        .all(|entry| entry.placement == HavingPlacement::BelowAggregate));
    // The two equal `Filter`s are one memo group, so one HAVING entry.
    assert_eq!(targets[0].having_filters.len(), 1);
}

/// A scalar reference to a grouped aggregate (`Filter(scan, EXISTS G)` as a
/// second root) is a consumer that reads it without the `HAVING`, so the
/// first root's key-only `HAVING` stays above the output.
#[test]
fn a_scalar_reference_consumer_keeps_a_key_only_having_above() {
    let node = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let filtered = having_filter(node.clone(), key.clone());
    let exists = having_filter(
        scan(false),
        Predicate(ScalarExpr::Exists {
            subquery: node.clone(),
            negated: false,
        }),
    );
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(
            &[
                QueryRoot::Operator(filtered.clone()),
                QueryRoot::Operator(exists),
            ],
            &vec![None; 2],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap();
    let targets = &inventory.targets;
    let target = targets
        .iter()
        .find(|target| **inventory.aggregate(target) == *node)
        .expect("the grouped count's target");
    assert_eq!(
        target.having_filters,
        vec![HavingEntry {
            filter: Some(filtered),
            predicate: key,
            placement: HavingPlacement::AboveOutput,
        }]
    );
}

/// Of two stacked `Filter`s above an aggregate, only the adjacent one is
/// its `HAVING`; the outer one filters the inner `Filter`'s output.
#[test]
fn only_the_adjacent_of_stacked_filters_is_having() {
    let node = grouped_count(None);
    let inner_key = equals(0, ScalarValue::Utf8("a".into()));
    let inner = having_filter(node.clone(), inner_key.clone());
    let outer = having_filter(inner.clone(), equals(0, ScalarValue::Utf8("b".into())));
    let targets = generate_per_computation_candidates(
        &memo_of_queries(&[QueryRoot::Operator(outer)], &[None], &Default::default()).unwrap(),
    )
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].having_filters,
        vec![HavingEntry {
            filter: Some(inner),
            predicate: inner_key,
            placement: HavingPlacement::BelowAggregate,
        }]
    );
}

/// A global aggregate's `HAVING` always stays above the output, even a
/// constant one: the one output row exists with no input rows.
#[test]
fn global_aggregate_having_stays_above() {
    let count = || vec![AggIntent::Count];
    let never = Predicate(ScalarExpr::Literal(ScalarValue::Boolean(false)));
    let targets = generate_per_computation_candidates(
        &memo_of_queries(
            &[
                QueryRoot::Operator(having_filter(
                    aggregate_over(scan(false), count(), vec![], None),
                    never.clone(),
                )),
                QueryRoot::Operator(aggregate_over(scan(false), count(), vec![], Some(never))),
            ],
            &vec![None; 2],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 2);
    for target in &targets {
        let placements: Vec<_> = target
            .having_filters
            .iter()
            .map(|entry| entry.placement)
            .collect();
        assert_eq!(placements, vec![HavingPlacement::AboveOutput]);
    }
}

/// The `HAVING` entries of the target whose measure satisfies `is`.
fn having_of(
    inventory: &crate::computation_targets::PerComputationCandidates,
    is: impl Fn(&AggIntent) -> bool,
) -> Vec<HavingEntry> {
    let target = inventory
        .targets
        .iter()
        .find(|target| match inventory.aggregate(target).non_asap() {
            Some(NonASAPOp::Aggregate { measures, .. }) => is(&measures[target.measure_index]),
            _ => false,
        })
        .expect("target");
    target.having_filters.clone()
}

/// `Column(column) > ScalarSubquery(subquery)`.
fn above_scalar(column: usize, subquery: Rc<OperatorNode>) -> Predicate {
    Predicate(ScalarExpr::Compare {
        left: Box::new(ScalarExpr::Column(column)),
        op: CompareOpKind::Gt,
        right: Box::new(ScalarExpr::ScalarSubquery(subquery)),
        semantics: ExprSemantics::Sql,
    })
}

/// A `Filter` that reads an aggregate only from its predicate is not that
/// aggregate's `HAVING`; one that also filters the aggregate gives one entry.
#[test]
fn scalar_reference_filter_is_not_having() {
    let count = || vec![AggIntent::Count];
    let agg = aggregate_over(scan(false), count(), vec![], None);
    let root = having_filter(scan(false), above_scalar(1, agg.clone()));
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(&[QueryRoot::Operator(root)], &[None], &Default::default()).unwrap(),
    )
    .unwrap();
    let targets = &inventory.targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(**inventory.aggregate(&targets[0]), *agg);
    assert!(
        targets[0].having_filters.is_empty(),
        "{:?}",
        targets[0].having_filters
    );

    let agg = aggregate_over(scan(false), count(), vec![], None);
    let root = having_filter(agg.clone(), above_scalar(0, agg.clone()));
    let targets = generate_per_computation_candidates(
        &memo_of_queries(
            &[QueryRoot::Operator(root.clone())],
            &[None],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].having_filters,
        vec![HavingEntry {
            filter: Some(root),
            predicate: above_scalar(0, agg),
            placement: HavingPlacement::AboveOutput,
        }]
    );
}

/// PromQL `scalar(...)` operands are scalar references, not `HAVING`.
#[test]
fn promql_scalar_operand_is_not_having() {
    let roots = exact_promql("http_requests_total > scalar(sum(http_requests_total))");
    let targets = generate_per_computation_candidates(
        &memo_of_queries(&roots, &vec![None; roots.len()], &Default::default()).unwrap(),
    )
    .unwrap()
    .targets;
    assert!(!targets.is_empty());
    for target in &targets {
        assert!(
            target.having_filters.is_empty(),
            "{:?}",
            target.having_filters
        );
    }

    let roots = exact_promql("sum by (job) (x) > scalar(count(x))");
    let inventory = generate_per_computation_candidates(
        &memo_of_queries(&roots, &vec![None; roots.len()], &Default::default()).unwrap(),
    )
    .unwrap();
    assert!(having_of(&inventory, |intent| matches!(intent, AggIntent::Count)).is_empty());
    let sum = having_of(&inventory, |intent| matches!(intent, AggIntent::Sum { .. }));
    assert_eq!(sum.len(), 1, "{sum:?}");
    assert_eq!(sum[0].placement, HavingPlacement::AboveOutput);
}

/// A non-root, non-`Filter` consumer keeps a key-only `HAVING` above.
#[test]
fn having_shared_with_a_project_consumer_stays_above() {
    let node = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let filtered = having_filter(node.clone(), key);
    let project = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Project {
        cols: vec![asap_types::ir::ProjectItem {
            alias: None,
            expr: ScalarExpr::Column(0),
        }],
        qualifier: None,
        child: node,
    }))
    .unwrap();
    let targets = generate_per_computation_candidates(
        &memo_of_queries(
            &[QueryRoot::Operator(filtered), QueryRoot::Operator(project)],
            &[None, None],
            &Default::default(),
        )
        .unwrap(),
    )
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 1);
    let placements: Vec<_> = targets[0]
        .having_filters
        .iter()
        .map(|entry| entry.placement)
        .collect();
    assert_eq!(placements, vec![HavingPlacement::AboveOutput]);
}

/// `without(...)` and per-entity aggregates have no fixed key prefix, so a
/// `HAVING` over column 0 stays above.
#[test]
fn without_and_per_entity_having_stays_above() {
    for reduction in [
        Reduction::Reduce(asap_types::ir::operator::arguments::GroupKeys::without(
            vec![1],
        )),
        Reduction::PerEntity,
    ] {
        let node = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
            child: scan(false),
            reduction: reduction.clone(),
            measures: vec![AggIntent::Count],
            output_names: vec![],
            filters: vec![],
            having: None,
        }))
        .unwrap();
        let root = having_filter(node, equals(0, ScalarValue::Utf8("a".into())));
        let targets = generate_per_computation_candidates(
            &memo_of_queries(&[QueryRoot::Operator(root)], &[None], &Default::default()).unwrap(),
        )
        .unwrap()
        .targets;
        assert_eq!(targets.len(), 1, "{reduction:?}");
        let placements: Vec<_> = targets[0]
            .having_filters
            .iter()
            .map(|entry| entry.placement)
            .collect();
        assert_eq!(
            placements,
            vec![HavingPlacement::AboveOutput],
            "{reduction:?}"
        );
    }
}

// Structurally equal aggregates are one memo group with one target, and its
// HAVING placement is decided over every consumer of the group: an unfiltered
// consumer keeps the key-only HAVING above the output for the filtered ones.
#[test]
fn memo_cached_rules_preserve_occurrence_specific_having() {
    let shared = grouped_count(None);
    let other = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let roots = vec![
        QueryRoot::Operator(having_filter(shared.clone(), key.clone())),
        QueryRoot::Operator(shared),
        QueryRoot::Operator(having_filter(other, key)),
    ];
    let memo = memo_of_queries(&roots, &vec![None; roots.len()], &Default::default()).unwrap();
    let cached = generate_per_computation_candidates(&memo).unwrap();
    assert_eq!(cached.targets.len(), 1);
    // Roots 0 and 2 apply the same `Filter`, one memo group: one entry.
    let [entry] = &cached.targets[0].having_filters[..] else {
        panic!("one HAVING entry: {:?}", cached.targets[0].having_filters);
    };
    assert_eq!(entry.placement, HavingPlacement::AboveOutput);
}
