//! Frontend-to-Pass-1 acceptance: candidate discovery precedes empirical selection.
use crate::pass1::per_computation_candidates::{
    computation_candidates_for_intent, generate_per_computation_candidates, HavingEntry,
    HavingPlacement, HowToComputeAggregateMeasure, PerComputationCandidateError,
};
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::properties::AccuracyTarget;
use asap_types::ir::scalar::{ColumnRef, CompareOpKind, ScalarValue};
use asap_types::ir::schema::{
    DataType, ExactKind, Field, FieldDataType, GroupingStrategy, Schema, SketchAlgorithm,
    SketchKind, SketchParams, SummaryUpdate,
};
use asap_types::ir::{
    ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot, ScalarExpr,
};
use asap_types::stages::PruneReason;
use std::rc::Rc;

fn approximate() -> AccuracyTarget {
    AccuracyTarget::max_error(0.05).failure_probability(0.01)
}
fn candidates_for(intent: AggIntent) -> Vec<HowToComputeAggregateMeasure> {
    computation_candidates_for_intent(&intent)
        .unwrap()
        .candidates
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
    let choices = candidates_for(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = candidates_for(AggIntent::Cardinality {
        cols: vec![0, 1],
        accuracy: approximate(),
    });
    assert!(!algorithms(&tuple).contains(&SketchAlgorithm::UnivMon));
}

/// Frequency moments retain exact execution and a universal sketch without certification.
#[test]
fn frequency_statistics_keep_universal_choices() {
    for intent in [
        AggIntent::FrequencyL2 {
            col: Some(0),
            accuracy: approximate(),
        },
        AggIntent::FrequencyEntropy {
            col: Some(0),
            accuracy: approximate(),
        },
    ] {
        let choices = candidates_for(intent);
        assert_eq!(choices[0], HowToComputeAggregateMeasure::Exact);
        assert_eq!(algorithms(&choices), vec![SketchAlgorithm::UnivMon]);
    }
}

/// An exact request cannot acquire an approximate sketch merely because one is
/// available; each pruned family records why.
#[test]
fn exact_quantile_stays_exact_and_approximate_keeps_both_families() {
    let choices = candidates_for(AggIntent::Quantile {
        col: Some(0),
        q: 0.99,
        accuracy: approximate(),
    });
    assert_eq!(
        algorithms(&choices),
        vec![SketchAlgorithm::Kll, SketchAlgorithm::DDSketch]
    );
    let exact = computation_candidates_for_intent(&AggIntent::Quantile {
        col: Some(0),
        q: 0.99,
        accuracy: AccuracyTarget::exact(),
    })
    .unwrap();
    assert_eq!(exact.candidates, vec![HowToComputeAggregateMeasure::Exact]);
    assert_eq!(
        exact.pruned,
        vec![
            (
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::Kll),
                PruneReason::RequiresExact
            ),
            (
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::DDSketch),
                PruneReason::RequiresExact
            ),
        ]
    );
}

/// Scalar roots expose their producer targets; repeated references retain one target identity.
#[test]
fn scalar_root_producers_are_discovered_once() {
    let producer = aggregate(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    let roots = vec![
        QueryRoot::Scalar(ScalarExpr::ScalarSubquery(producer.clone())),
        QueryRoot::Operator(producer.clone()),
    ];
    let candidates = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    assert_eq!(candidates.roots.len(), 2);
    for root in &candidates.roots {
        root.validate_structure().unwrap();
    }
    assert_eq!(candidates.targets.len(), 1);
    assert_eq!(**candidates.aggregate(&candidates.targets[0]), *producer);
    assert!(producer.timing.is_none());
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
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
    assert!(candidates
        .targets
        .iter()
        .all(|target| candidates.aggregate(target).timing.is_none()));
}

/// Physical annotations, ASAP operators and invalid probability requirements
/// fail at the stage boundary.
#[test]
fn assigned_physical_annotation_asap_input_and_invalid_accuracy_are_rejected() {
    let mut producer = (*aggregate(AggIntent::Count {
        accuracy: approximate(),
    }))
    .clone();
    producer.timing = Some(asap_types::ir::properties::ExecutionTiming::QueryTime);
    assert!(matches!(
        generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
                Rc::new(producer.clone())
            )])
        ),
        Err(PerComputationCandidateError::AssignedPhysicalAnnotation)
    ));
    let mut stored = (*aggregate(AggIntent::Count {
        accuracy: approximate(),
    }))
    .clone();
    stored.materialization =
        asap_types::physical::Materialization::MatAtQuery(asap_types::physical::Medium::Memory);
    assert!(matches!(
        generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
                Rc::new(stored.clone())
            )])
        ),
        Err(PerComputationCandidateError::AssignedPhysicalAnnotation)
    ));
    // An annotated node below an unannotated root is rejected too.
    for annotated in [producer, stored] {
        let root = having_filter(Rc::new(annotated), always());
        assert!(matches!(
            generate_per_computation_candidates(
                &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
                    root
                )])
            ),
            Err(PerComputationCandidateError::AssignedPhysicalAnnotation)
        ));
    }
    let summary = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan(false),
        family: FieldDataType::Sketch(
            SketchKind::new(
                SketchAlgorithm::Kll,
                SketchParams::Kll {
                    k: 200,
                    failure_probability: None,
                },
            ),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    assert!(matches!(
        generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
                summary
            )])
        ),
        Err(PerComputationCandidateError::ContainsAsap)
    ));
    let epsilon_delta =
        |epsilon, delta| AccuracyTarget::max_error(epsilon).failure_probability(delta);
    for target in [
        AccuracyTarget::max_error(f64::NAN),
        epsilon_delta(0.0, 0.01),
        epsilon_delta(-0.1, 0.01),
        epsilon_delta(1.0, 0.01),
        epsilon_delta(f64::INFINITY, 0.01),
        epsilon_delta(0.1, 0.0),
        epsilon_delta(0.1, 1.0),
    ] {
        assert!(
            matches!(
                computation_candidates_for_intent(&AggIntent::Count {
                    accuracy: target.clone()
                }),
                Err(PerComputationCandidateError::InvalidAccuracy {
                    root: None,
                    measure: None
                })
            ),
            "{target:?}"
        );
    }
    let invalid = aggregate_over(
        scan(false),
        vec![
            AggIntent::Count {
                accuracy: approximate(),
            },
            AggIntent::Count {
                accuracy: epsilon_delta(1.5, 0.01),
            },
        ],
        vec![],
        None,
    );
    let error = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(invalid)]),
    )
    .unwrap_err();
    assert!(
        matches!(
            &error,
            PerComputationCandidateError::InvalidAccuracy {
                root: Some(root),
                measure: Some(1)
            } if root == "0"
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("root 0, measure 1 "), "{error}");
}

/// TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = candidates_for(AggIntent::TopK {
        k: 10,
        accuracy: approximate(),
    });
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
    let loose = candidates_for(AggIntent::Count {
        accuracy: approximate(),
    });
    let tight = candidates_for(AggIntent::Count {
        accuracy: AccuracyTarget::max_error(0.0001).failure_probability(0.0001),
    });
    assert_eq!(loose, tight);
}

/// Each measure of a multi-measure aggregate is its own target; a measure whose
/// only realization is its exact accumulator is still a target.
#[test]
fn multi_measure_aggregate_has_one_target_per_measure() {
    let node = aggregate_over(
        scan(false),
        vec![
            AggIntent::Sum { col: Some(1) },
            AggIntent::Count {
                accuracy: approximate(),
            },
        ],
        vec![],
        None,
    );
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(node)]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(node)])
    )
    .unwrap()
    .targets
    .is_empty());
}

/// A filtered SQL count keeps its executable accumulator and records its
/// filter, which composition applies to the state.
#[test]
fn filtered_measure_keeps_candidates_and_records_its_filter() {
    let count = || AggIntent::Count {
        accuracy: approximate(),
    };
    let filtered = aggregate_over(
        scan(false),
        vec![count(), count()],
        vec![Some(always()), None],
        None,
    );
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(filtered)]),
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

/// Target `target`'s own ways to compute and the ones Pass 1 pruned.
fn choices_of(
    inventory: &crate::pass1::per_computation_candidates::PerComputationCandidates,
    target: usize,
) -> crate::pass1::per_computation_candidates::ComputationChoices {
    crate::pass1::per_computation_candidates::ComputationChoices {
        candidates: inventory.targets[target].ways_to_compute.clone(),
        pruned: inventory.pruned_of_target(target, false),
    }
}

fn assert_sql_count_choices(
    choices: &crate::pass1::per_computation_candidates::ComputationChoices,
) {
    assert_eq!(
        choices.candidates,
        vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![
            ExactKind::Count
        ])]
    );
    assert_eq!(
        choices.pruned,
        [SketchAlgorithm::Cms, SketchAlgorithm::UnivMon]
            .into_iter()
            .map(|algorithm| (
                HowToComputeAggregateMeasure::Summary(algorithm),
                PruneReason::NoRelationalCountSketchRealization,
            ))
            .collect::<Vec<_>>()
    );
}

fn quantile() -> AggIntent {
    AggIntent::Quantile {
        col: Some(1),
        q: 0.5,
        accuracy: approximate(),
    }
}

fn grouped_quantile(having: Option<Predicate>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child: scan(false),
        reduction: Reduction::by(vec![0]),
        measures: vec![quantile()],
        output_names: vec![],
        filters: vec![],
        having,
    }))
    .unwrap()
}

/// `count(*) by (src_ip)`, the form the SQL frontend gives `GROUP BY src_ip`.
fn grouped_count(having: Option<Predicate>) -> Rc<OperatorNode> {
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child: scan(false),
        reduction: Reduction::by(vec![0]),
        measures: vec![AggIntent::Count {
            accuracy: approximate(),
        }],
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
            root.clone(),
        )]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(having_filter(grouped_count(None), value())),
            QueryRoot::Operator(grouped_count(Some(value()))),
        ]),
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

/// `avg`'s one exact option is sum and count states when its input is never
/// null, and plain `Exact` when that rewrite is pruned.
#[test]
fn avg_rewrites_to_sum_and_count_states() {
    let avg = || vec![AggIntent::Avg { col: None }];
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(aggregate_over(scan(false), avg(), vec![], None)),
            QueryRoot::Operator(aggregate_over(scan(true), avg(), vec![], None)),
        ]),
    )
    .unwrap();
    let targets = &inventory.targets;
    assert_eq!(
        targets[0].ways_to_compute,
        vec![HowToComputeAggregateMeasure::ExactAccumulator(vec![
            ExactKind::Sum,
            ExactKind::Count
        ])]
    );
    assert_eq!(
        targets[1].ways_to_compute,
        vec![HowToComputeAggregateMeasure::Exact]
    );
    assert_eq!(
        inventory.pruned_of_target(1, false),
        vec![(
            HowToComputeAggregateMeasure::ExactAccumulator(vec![ExactKind::Sum, ExactKind::Count]),
            PruneReason::NullableAvgInput
        )]
    );
}

/// A key-only `HAVING` above a shared aggregate stays above the output when
/// another consumer reads the aggregate without it.
#[test]
fn having_on_one_consumer_of_a_shared_aggregate_stays_above() {
    let node = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let filtered = having_filter(node.clone(), key.clone());
    let targets = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(filtered.clone()),
            QueryRoot::Operator(node.clone()),
        ]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(first),
            QueryRoot::Operator(second),
        ]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(filtered.clone()),
            QueryRoot::Operator(exists),
        ]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(outer)]),
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
    let count = || {
        vec![AggIntent::Count {
            accuracy: approximate(),
        }]
    };
    let never = Predicate(ScalarExpr::Literal(ScalarValue::Boolean(false)));
    let targets = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(having_filter(
                aggregate_over(scan(false), count(), vec![], None),
                never.clone(),
            )),
            QueryRoot::Operator(aggregate_over(scan(false), count(), vec![], Some(never))),
        ]),
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

/// #509 Example 1 Q2 under the default exact target: the top-k's one option is plain
/// `Exact`, its heap summaries pruned `RequiresExact`; `sum_over_time` gets
/// its exact Sum accumulator.
#[test]
fn example_1_exact_q2_prunes_topk_heaps() {
    let roots = exact_promql("topk by (job) (10, sum_over_time(http_requests_total[1m]))");
    let candidates = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    let found: Vec<_> = candidates
        .targets
        .iter()
        .enumerate()
        .map(|(index, _)| choices_of(&candidates, index))
        .collect();
    let topk = found
        .iter()
        .find(|choices| choices.candidates == [HowToComputeAggregateMeasure::Exact])
        .expect("top-k target");
    assert_eq!(
        topk.pruned,
        vec![
            (
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CmsWithHeap),
                PruneReason::RequiresExact
            ),
            (
                HowToComputeAggregateMeasure::Summary(SketchAlgorithm::CountSketchWithHeap),
                PruneReason::RequiresExact
            ),
        ]
    );
    assert!(found.iter().any(|choices| choices.candidates
        == [HowToComputeAggregateMeasure::ExactAccumulator(vec![
            ExactKind::Sum
        ])]
        && choices.pruned.is_empty()));
    assert_eq!(found.len(), 2, "{found:#?}");
}

/// The `HAVING` entries of the target whose measure satisfies `is`.
fn having_of(
    inventory: &crate::pass1::per_computation_candidates::PerComputationCandidates,
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
    let count = || {
        vec![AggIntent::Count {
            accuracy: approximate(),
        }]
    };
    let agg = aggregate_over(scan(false), count(), vec![], None);
    let root = having_filter(scan(false), above_scalar(1, agg.clone()));
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(root)]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
            root.clone(),
        )]),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&roots),
    )
    .unwrap();
    assert!(having_of(&inventory, |intent| matches!(
        intent,
        AggIntent::Count { .. }
    ))
    .is_empty());
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
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[
            QueryRoot::Operator(filtered),
            QueryRoot::Operator(project),
        ]),
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
        Reduction::Reduce(
            asap_types::ir::operator::operator_properties::GroupKeys::without(vec![1]),
        ),
        Reduction::PerEntity,
    ] {
        let node = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
            child: scan(false),
            reduction: reduction.clone(),
            measures: vec![AggIntent::Count {
                accuracy: approximate(),
            }],
            output_names: vec![],
            filters: vec![],
            having: None,
        }))
        .unwrap();
        let root = having_filter(node, equals(0, ScalarValue::Utf8("a".into())));
        let targets = generate_per_computation_candidates(
            &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(root)]),
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

/// A workload candidate replaces only chosen targets, derives the summary's
/// coverage from its subtree, and keeps unchosen plans as they are, now
/// with an exact guarantee.
#[test]
fn composed_candidate_replaces_chosen_target_with_summary_evaluation() {
    use crate::pass1::per_computation_candidates::compose_logical_candidate;
    let producer = aggregate(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
            producer.clone(),
        )]),
    )
    .unwrap();
    let exact = compose_logical_candidate(&inventory, &[0]).unwrap();
    let QueryRoot::Operator(kept) = &exact[0] else {
        panic!("operator root expected")
    };
    assert_eq!(kept.operator.kind_name(), producer.operator.kind_name());
    assert_eq!(kept.schema, producer.schema);
    assert!(kept.guarantee.as_ref().is_some_and(|g| g.is_exact()));

    let hll = compose_logical_candidate(&inventory, &[1]).unwrap();
    let QueryRoot::Operator(estimate) = &hll[0] else {
        panic!("operator root expected")
    };
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
        panic!("summary evaluation expected")
    };
    // The state takes every row of its source: one unrestricted selection box.
    let coverage = summary_input.coverage().unwrap();
    assert!(OperatorNode::reachable(coverage.definition()).iter().any(|node| matches!(
        node.non_asap(),
        Some(NonASAPOp::Scan { source: Source::Table { table_ref }, .. }) if table_ref == "flows"
    )));
    assert_eq!(coverage.selection().len(), 1);
    assert!(
        coverage.selection()[0].columns().is_empty()
            && coverage.selection()[0].relative_time().is_none()
    );
    hll[0].validate_structure().unwrap();
    assert!(compose_logical_candidate(&inventory, &[99]).is_err());
}

/// The one root of the workload candidate composed from `roots` and `choice`.
fn composed(roots: Vec<Rc<OperatorNode>>, choice: &[usize]) -> Vec<Rc<OperatorNode>> {
    use crate::pass1::per_computation_candidates::compose_logical_candidate;
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(
            &roots
                .into_iter()
                .map(QueryRoot::Operator)
                .collect::<Vec<_>>(),
        ),
    )
    .unwrap();
    compose_logical_candidate(&inventory, choice)
        .unwrap()
        .into_iter()
        .map(|root| {
            root.validate_structure().unwrap();
            match root {
                QueryRoot::Operator(node) => node,
                QueryRoot::Scalar(_) => panic!("operator root expected"),
            }
        })
        .collect()
}
/// Output columns as (name, type, nullable).
fn columns(node: &OperatorNode) -> Vec<(String, FieldDataType, bool)> {
    node.schema
        .fields
        .iter()
        .map(|field| (field.name.clone(), field.dtype.clone(), field.nullable))
        .collect()
}
// Quantile readouts restore the aggregate's output names with an identity projection.
fn without_output_aliases(node: &OperatorNode) -> &OperatorNode {
    if let Some(NonASAPOp::Project { cols, child, .. }) = node.non_asap() {
        assert!(cols
            .iter()
            .enumerate()
            .all(|(index, item)| item.expr == ScalarExpr::Column(index)));
        without_output_aliases(child)
    } else {
        node
    }
}
fn summary_agg(node: &OperatorNode) -> &ASAPOp {
    let node = without_output_aliases(node);
    match node.asap() {
        Some(ASAPOp::SummaryEstimate { summary_input, .. }) => summary_agg(summary_input),
        Some(ASAPOp::FinalizeExactAccumulator { child }) => summary_agg(child),
        Some(agg @ ASAPOp::SummaryAgg { .. }) => agg,
        _ => panic!("summary evaluation expected, got {node:#?}"),
    }
}

/// A summary is sized when the workload candidate is composed, for its
/// measure's accuracy requirement.
#[test]
fn composed_summary_is_sized_for_its_measure() {
    use asap_accuracy::test_support::default_size_params;
    let intent = quantile();
    let [root] = <[_; 1]>::try_from(composed(vec![aggregate(intent.clone())], &[1])).unwrap();
    let ASAPOp::SummaryAgg {
        family: FieldDataType::Sketch(kind, _),
        ..
    } = summary_agg(&root)
    else {
        panic!("sketch state expected")
    };
    assert_eq!(*kind.algorithm(), SketchAlgorithm::Kll);
    assert_eq!(
        kind.params(),
        &default_size_params(SketchAlgorithm::Kll, &intent, 0.05, 0.01)
    );
}

/// A measure filter becomes the state's filter; a key-only `HAVING` applied
/// by every consumer moves below the state; a value `HAVING` stays a `Filter`
/// above the estimate.
#[test]
fn composed_filters_and_having() {
    let kll = 1;
    let filtered = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child: scan(false),
        reduction: Reduction::by(vec![0]),
        measures: vec![quantile()],
        output_names: vec![],
        filters: vec![Some(always())],
        having: None,
    }))
    .unwrap();
    let [root] = <[_; 1]>::try_from(composed(vec![filtered.clone()], &[kll])).unwrap();
    assert!(
        matches!(summary_agg(&root), ASAPOp::SummaryAgg { filter: Some(f), child, .. }
            if *f == always() && child.non_asap().is_some_and(|op| matches!(op, NonASAPOp::Scan { .. })))
    );
    assert_eq!(columns(&root)[0], columns(&filtered)[0]);

    let key = equals(0, ScalarValue::Utf8("a".into()));
    let [root] = <[_; 1]>::try_from(composed(
        vec![having_filter(grouped_quantile(None), key.clone())],
        &[kll],
    ))
    .unwrap();
    let ASAPOp::SummaryAgg { child, filter, .. } = summary_agg(&root) else {
        unreachable!()
    };
    assert_eq!(*filter, None);
    assert!(matches!(child.non_asap(), Some(NonASAPOp::Filter { pred, .. }) if *pred == key));

    let value = || equals(1, ScalarValue::Int64(10));
    for root in composed(
        vec![
            having_filter(grouped_quantile(None), value()),
            grouped_quantile(Some(value())),
        ],
        &[kll, kll],
    ) {
        let Some(NonASAPOp::Filter { pred, child }) = root.non_asap() else {
            panic!("value HAVING above the estimate expected, got {root:#?}")
        };
        assert_eq!(*pred, value());
        assert!(matches!(
            without_output_aliases(child).asap(),
            Some(ASAPOp::SummaryEstimate { .. })
        ));
    }
}

/// SQL `avg` joins its finalized `sum` and `count` accumulator states on the
/// group keys and divides them.
#[test]
fn composed_avg_divides_sum_and_count_states() {
    let avg = aggregate_over(
        scan(false),
        vec![AggIntent::Avg { col: Some(1) }],
        vec![],
        None,
    );
    let [root] = <[_; 1]>::try_from(composed(vec![avg.clone()], &[0])).unwrap();
    // The quotient is renamed to the aggregate's column.
    assert_eq!(columns(&root), columns(&avg));
    let join = OperatorNode::reachable(&root)
        .into_iter()
        .find(|node| matches!(node.non_asap(), Some(NonASAPOp::Join { .. })))
        .unwrap_or_else(|| panic!("sum joined with count expected, got {root:#?}"));
    let Some(NonASAPOp::Join { left, right, .. }) = join.non_asap() else {
        unreachable!()
    };
    for (side, kind) in [(left, ExactKind::Sum), (right, ExactKind::Count)] {
        assert!(matches!(
            summary_agg(side),
            ASAPOp::SummaryAgg { family: FieldDataType::ExactAggregate(found, _), .. } if *found == kind
        ));
    }
}

/// Each measure of a multi-measure aggregate is substituted on its own and the
/// measures are joined back on the group keys, with the aggregate's columns.
#[test]
fn composed_multi_measure_aggregate_joins_its_measures() {
    for reduction in [Reduction::by(vec![0]), Reduction::by(vec![])] {
        let node = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
            child: scan(false),
            reduction,
            measures: vec![
                AggIntent::Sum { col: Some(1) },
                quantile(),
                AggIntent::StdDev {
                    col: Some(1),
                    population: true,
                },
            ],
            output_names: vec![],
            filters: vec![],
            having: None,
        }))
        .unwrap();
        let [root] = <[_; 1]>::try_from(composed(vec![node.clone()], &[0, 1])).unwrap();
        assert!(matches!(root.non_asap(), Some(NonASAPOp::Project { .. })));
        let reached = OperatorNode::reachable(&root);
        let joins = reached
            .iter()
            .filter(|node| matches!(node.non_asap(), Some(NonASAPOp::Join { .. })))
            .count();
        assert_eq!(joins, 2);
        assert!(reached
            .iter()
            .any(|node| matches!(node.asap(), Some(ASAPOp::SummaryEstimate { .. }))));
        let names = |node: &OperatorNode| -> Vec<_> {
            columns(node)
                .into_iter()
                .map(|(name, dtype, _)| (name, dtype))
                .collect()
        };
        assert_eq!(names(&root), names(&node));
    }
}

/// A measure left exact beside a sketched one keeps its output name: the
/// split exact aggregate names its one measure as the target named it, so the
/// executor finds one name per measure.
#[test]
fn composed_mixed_exact_and_sketch_measures_keep_output_names() {
    let quantile = |q| AggIntent::Quantile {
        col: Some(1),
        q,
        accuracy: approximate(),
    };
    let node = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child: scan(false),
        reduction: Reduction::by(vec![]),
        measures: vec![quantile(0.5), quantile(0.9)],
        output_names: vec!["p50".into(), "p90".into()],
        filters: vec![],
        having: None,
    }))
    .unwrap();
    assert_eq!(
        candidates_for(quantile(0.5))[0],
        HowToComputeAggregateMeasure::Exact
    );
    // p50 exact, p90 from the first sketch family.
    let [root] = <[_; 1]>::try_from(composed(vec![node.clone()], &[0, 1])).unwrap();
    let exact_parts: Vec<_> = OperatorNode::reachable(&root)
        .into_iter()
        .filter_map(|node| match node.non_asap() {
            Some(NonASAPOp::Aggregate {
                measures,
                output_names,
                ..
            }) => Some((measures.len(), output_names.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(exact_parts, vec![(1, vec!["p50".to_string()])]);
    assert_eq!(columns(&root), columns(&node));
}

/// A summary estimate carries its sized family's bound; an exact accumulator
/// is exact; anything computed from an approximate value, a value `HAVING`
/// included, has no guarantee.
#[test]
fn composed_guarantees() {
    let value = equals(1, ScalarValue::Int64(10));
    let roots = composed(
        vec![
            aggregate(AggIntent::Cardinality {
                cols: vec![0],
                accuracy: approximate(),
            }),
            aggregate(AggIntent::Sum { col: Some(1) }),
            having_filter(grouped_quantile(None), value),
        ],
        &[1, 0, 1],
    );
    let guarantee = |node: &OperatorNode| node.guarantee.clone();
    let hll = guarantee(&roots[0]).expect("HLL bound");
    assert!(!hll.is_exact());
    assert!(guarantee(&roots[1]).is_some_and(|g| g.is_exact()));
    let Some(NonASAPOp::Filter { child, .. }) = roots[2].non_asap() else {
        panic!("value HAVING above the estimate expected")
    };
    assert!(guarantee(child).is_some());
    assert!(guarantee(&roots[2]).is_none());
}

/// The aggregate's own value `having` stays a `Filter` above the output, and
/// a subquery in it reads the subquery's substitution.
#[test]
fn own_having_subquery_is_substituted() {
    use crate::pass1::per_computation_candidates::compose_logical_candidate;
    let inner = aggregate(quantile());
    let outer = grouped_quantile(Some(above_scalar(1, inner.clone())));
    let inventory = generate_per_computation_candidates(
        &crate::pass2::memo::DagOfLogicalMemoGroups::from_roots(&[QueryRoot::Operator(
            outer.clone(),
        )]),
    )
    .unwrap();
    assert_eq!(inventory.targets.len(), 2);
    let kll = 1;
    let composed = compose_logical_candidate(&inventory, &[kll, kll]).unwrap();
    let QueryRoot::Operator(root) = &composed[0] else {
        panic!("operator root expected")
    };
    let Some(NonASAPOp::Filter { pred, child }) = root.non_asap() else {
        panic!("own HAVING above the estimate expected, got {root:#?}")
    };
    assert!(matches!(
        without_output_aliases(child).asap(),
        Some(ASAPOp::SummaryEstimate { .. })
    ));
    let [subquery] = <[_; 1]>::try_from(pred.0.operator_refs()).unwrap();
    assert!(
        matches!(
            without_output_aliases(subquery).asap(),
            Some(ASAPOp::SummaryEstimate { .. })
        ),
        "{subquery:#?}"
    );
    assert!(OperatorNode::reachable(root)
        .iter()
        .all(|node| !matches!(node.non_asap(), Some(NonASAPOp::Aggregate { .. }))));
}

/// A per-series `avg` over PromQL rows that carry the series identity
/// (`$promql_series_identity` after the sample value) composes from its sum
/// and count states: the summed value is found by role, not as the last
/// column.
#[test]
fn per_series_avg_with_series_identity_composes_from_sum_and_count() {
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    let [QueryRoot::Operator(root)] = &exact_promql("avg_over_time(m[5m])")[..] else {
        panic!("one operator root");
    };
    let root = with_promql_series_identity(root).unwrap();
    let [composed] = <[_; 1]>::try_from(composed(vec![root.clone()], &[0])).unwrap();
    assert_eq!(columns(&composed), columns(&root));
}

/// Exact per-series `rate` keeps every label and retypes the value column to the Rate state.
#[test]
fn per_series_rate_keeps_labels_and_retypes_its_state() {
    use asap_types::ir::schema::ExactParams;
    let [QueryRoot::Operator(root)] = &exact_promql("rate(m{job=\"api\"}[5m])")[..] else {
        panic!("one operator root");
    };
    let [out] = <[_; 1]>::try_from(composed(vec![root.clone()], &[0])).unwrap();
    let rate = FieldDataType::ExactAggregate(ExactKind::Rate, ExactParams::Rate);
    let ASAPOp::SummaryAgg {
        family, reduction, ..
    } = summary_agg(&out)
    else {
        unreachable!()
    };
    assert_eq!(*family, rate);
    assert_eq!(*reduction, Reduction::PerEntity);
    let state = OperatorNode::reachable(&out)
        .into_iter()
        .find(|node| matches!(node.asap(), Some(ASAPOp::SummaryAgg { .. })))
        .unwrap();
    // The state replaces the sample value in place, named `state`; the
    // finalized output has the rate's own columns.
    let label = Field::plain("job", DataType::Utf8, true);
    assert_eq!(
        columns(&state),
        [
            (
                "ts".into(),
                FieldDataType::Plain(DataType::Timestamp),
                false
            ),
            ("state".into(), rate, false),
            (label.name, label.dtype, label.nullable),
        ]
    );
    assert_eq!(state.schema.time_index, Some(0));
    assert_eq!(out.schema, root.schema);
}

/// Exact grouped `avg` composes from sum and count with the aggregate's whole schema.
#[test]
fn grouped_avg_composition_preserves_the_whole_schema() {
    let [QueryRoot::Operator(root)] = &exact_promql("avg by (job) (m)")[..] else {
        panic!("one operator root");
    };
    let [out] = <[_; 1]>::try_from(composed(vec![root.clone()], &[0])).unwrap();
    assert!(OperatorNode::reachable(&out)
        .iter()
        .any(|node| matches!(node.asap(), Some(ASAPOp::FinalizeExactAccumulator { .. }))));
    assert_eq!(out.schema, root.schema);
}

// Structurally equal aggregates are one memo group with one target, and its
// HAVING placement is decided over every consumer of the group: an unfiltered
// consumer keeps the key-only HAVING above the output for the filtered ones.
#[test]
fn memo_cached_rules_preserve_occurrence_specific_having() {
    use crate::pass2::memo::DagOfLogicalMemoGroups;
    let shared = grouped_count(None);
    let other = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let roots = vec![
        QueryRoot::Operator(having_filter(shared.clone(), key.clone())),
        QueryRoot::Operator(shared),
        QueryRoot::Operator(having_filter(other, key)),
    ];
    let memo = DagOfLogicalMemoGroups::from_roots(&roots);
    let cached = generate_per_computation_candidates(&memo).unwrap();
    assert_eq!(cached.targets.len(), 1);
    // Roots 0 and 2 apply the same `Filter`, one memo group: one entry.
    let [entry] = &cached.targets[0].having_filters[..] else {
        panic!("one HAVING entry: {:?}", cached.targets[0].having_filters);
    };
    assert_eq!(entry.placement, HavingPlacement::AboveOutput);
}
