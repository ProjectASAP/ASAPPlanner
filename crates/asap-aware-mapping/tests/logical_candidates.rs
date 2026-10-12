//! Frontend-to-Pass-1 acceptance: candidate discovery precedes empirical selection.
use asap_aware_mapping::logical_candidates::{
    enumerate_local_logical_candidates, local_candidates_for_intent, HavingEntry, HavingPlacement,
    LocalCandidate, LogicalCandidateError, PruneReason,
};
use asap_types::{
    ir::operator_properties::{Reduction, Source},
    ir::{
        ASAPOp, ExprSemantics, NonASAPOp, Operator, OperatorNode, Predicate, QueryRoot, ScalarExpr,
    },
    post_asap::{
        ExactKind, ExactParams, GroupingStrategy, SketchAlgorithm, SketchKind, SketchParams,
        SummaryUpdate,
    },
    pre_asap::{
        AggIntent, ColumnRef, CompareOpKind, DataType, Field, FieldDataType, ScalarValue, Schema,
    },
    types::AccuracyTarget,
};
use std::rc::Rc;

fn approximate() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.05,
        delta: 0.01,
    }
}
fn alternatives(intent: AggIntent) -> Vec<LocalCandidate> {
    local_candidates_for_intent(&intent).unwrap().alternatives
}
fn algorithms(choices: &[LocalCandidate]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            LocalCandidate::Summary(algorithm) => Some(algorithm.clone()),
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
fn cardinality_keeps_exact_specialized_and_universal_alternatives() {
    let choices = alternatives(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    assert_eq!(choices[0], LocalCandidate::Exact);
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = alternatives(AggIntent::Cardinality {
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
        let choices = alternatives(intent);
        assert_eq!(choices[0], LocalCandidate::Exact);
        assert_eq!(algorithms(&choices), vec![SketchAlgorithm::UnivMon]);
    }
}

/// An exact request cannot acquire an approximate sketch merely because one is
/// available; each pruned family records why.
#[test]
fn exact_quantile_stays_exact_and_approximate_keeps_both_families() {
    let choices = alternatives(AggIntent::Quantile {
        col: Some(0),
        q: 0.99,
        accuracy: approximate(),
    });
    assert_eq!(
        algorithms(&choices),
        vec![SketchAlgorithm::Kll, SketchAlgorithm::DDSketch]
    );
    let exact = local_candidates_for_intent(&AggIntent::Quantile {
        col: Some(0),
        q: 0.99,
        accuracy: AccuracyTarget::Exact,
    })
    .unwrap();
    assert_eq!(exact.alternatives, vec![LocalCandidate::Exact]);
    assert_eq!(
        exact.pruned,
        vec![
            (
                LocalCandidate::Summary(SketchAlgorithm::Kll),
                PruneReason::RequiresExact
            ),
            (
                LocalCandidate::Summary(SketchAlgorithm::DDSketch),
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
        (
            "scalar",
            QueryRoot::Scalar(ScalarExpr::ScalarSubquery(producer.clone())),
        ),
        ("relation", QueryRoot::Operator(producer.clone())),
    ];
    let candidates = enumerate_local_logical_candidates(roots).unwrap();
    assert_eq!(candidates.roots.len(), 2);
    for (_, root) in &candidates.roots {
        root.validate_structure().unwrap();
    }
    assert_eq!(candidates.targets.len(), 1);
    assert!(Rc::ptr_eq(&candidates.targets[0].target, &producer));
    assert!(producer.timing.is_none());
    assert!(producer.guarantee.is_none());
    producer.validate_structure().unwrap();
}

/// #509 Example 1 Q1 is exact: each target's one exact option is its exact
/// accumulator state (grouped sum, rate); no plain exact form and no summary.
/// One ImplicitExact PromQL query lowered through the workload frontend.
fn implicit_exact_promql(query: &str) -> Vec<QueryRoot> {
    use asap_types::workload::{
        AccuracyRequirement, BatchEntry, PlanningWorkload, Query, QueryLanguage, QueryRequirements,
        QueryWorkload,
    };
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::ImplicitExact,
                    ..Default::default()
                },
                predictability: Default::default(),
                invocations: 1,
                execute_at: None,
                time_selection: Default::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(asap_types::workload::DataWorkload {
            data_ingestion_interval: asap_types::workload::Evidence {
                value: Some(asap_types::workload::DurationMs(1000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    asap_frontend_promql::lower_promql_query_workload(&workload, 0).unwrap()
}

#[test]
fn example_1_exact_q1_keeps_exact_accumulators_only() {
    let roots = implicit_exact_promql("sum by (job) (rate(http_requests_total[1m]))");
    let candidates =
        enumerate_local_logical_candidates(roots.into_iter().enumerate().collect()).unwrap();
    let found: Vec<_> = candidates
        .targets
        .iter()
        .map(|target| target.choices.alternatives.clone())
        .collect();
    assert_eq!(
        found,
        vec![
            vec![LocalCandidate::ExactAccumulator {
                kind: ExactKind::Sum,
                params: ExactParams::Sum
            }],
            vec![LocalCandidate::ExactAccumulator {
                kind: ExactKind::Rate,
                params: ExactParams::Rate
            }],
        ]
    );
    assert!(candidates
        .targets
        .iter()
        .all(|target| target.target.timing.is_none()));
}

/// Physical annotations, ASAP operators and invalid probability requirements
/// fail at the stage boundary.
#[test]
fn assigned_physical_annotation_asap_input_and_invalid_accuracy_are_rejected() {
    let mut producer = (*aggregate(AggIntent::Count {
        accuracy: approximate(),
    }))
    .clone();
    producer.timing = Some(asap_types::post_asap::ExecutionTiming::QueryTime);
    assert!(matches!(
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(Rc::new(producer)))]),
        Err(LogicalCandidateError::AssignedPhysicalAnnotation)
    ));
    let mut stored = (*aggregate(AggIntent::Count {
        accuracy: approximate(),
    }))
    .clone();
    stored.materialization =
        asap_types::post_asap::Materialization::MatAtQuery(asap_types::post_asap::Medium::Memory);
    assert!(matches!(
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(Rc::new(stored)))]),
        Err(LogicalCandidateError::AssignedPhysicalAnnotation)
    ));
    let summary = OperatorNode::new_shared(Operator::ASAP(ASAPOp::SummaryAgg {
        child: scan(false),
        family: FieldDataType::Sketch(
            SketchKind::new(SketchAlgorithm::Kll, SketchParams::Kll { k: 200 }),
            Default::default(),
        ),
        input: SummaryUpdate::column(ColumnRef::SampleValue),
        reduction: Reduction::by(vec![]),
        grouping: GroupingStrategy::default(),
        filter: None,
    }))
    .unwrap();
    assert!(matches!(
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(summary))]),
        Err(LogicalCandidateError::ContainsAsap)
    ));
    let epsilon_delta = |epsilon, delta| AccuracyTarget::EpsilonDelta { epsilon, delta };
    for target in [
        AccuracyTarget::Epsilon(f64::NAN),
        epsilon_delta(0.0, 0.01),
        epsilon_delta(-0.1, 0.01),
        epsilon_delta(1.0, 0.01),
        epsilon_delta(f64::INFINITY, 0.01),
        epsilon_delta(0.1, 0.0),
        epsilon_delta(0.1, 1.0),
    ] {
        assert!(
            matches!(
                local_candidates_for_intent(&AggIntent::Count {
                    accuracy: target.clone()
                }),
                Err(LogicalCandidateError::InvalidAccuracy {
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
    let error =
        enumerate_local_logical_candidates(vec![("q", QueryRoot::Operator(invalid))]).unwrap_err();
    assert!(
        matches!(
            &error,
            LogicalCandidateError::InvalidAccuracy {
                root: Some(root),
                measure: Some(1)
            } if root == "\"q\""
        ),
        "{error:?}"
    );
}

/// Local TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = alternatives(AggIntent::TopK {
        k: 10,
        accuracy: approximate(),
    });
    assert_eq!(choices[0], LocalCandidate::Exact);
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
    let loose = alternatives(AggIntent::Count {
        accuracy: approximate(),
    });
    let tight = alternatives(AggIntent::Count {
        accuracy: AccuracyTarget::EpsilonDelta {
            epsilon: 0.0001,
            delta: 0.0001,
        },
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
    let candidates = enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(node))])
        .unwrap()
        .targets;
    assert_eq!(
        candidates
            .iter()
            .map(|target| target.measure)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        candidates[0].choices.alternatives,
        vec![LocalCandidate::ExactAccumulator {
            kind: ExactKind::Sum,
            params: ExactParams::Sum
        }]
    );
    assert_eq!(
        algorithms(&candidates[1].choices.alternatives),
        vec![
            SketchAlgorithm::Cms,
            SketchAlgorithm::CountSketch,
            SketchAlgorithm::UnivMon
        ]
    );
}

/// An aggregate with no realization besides exact execution is not a target.
#[test]
fn aggregate_without_realization_is_not_a_target() {
    let node = aggregate(AggIntent::StdDev {
        col: Some(1),
        population: true,
    });
    assert!(
        enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(node))])
            .unwrap()
            .targets
            .is_empty()
    );
}

/// A filtered measure keeps its accumulator and summaries and records its
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
    let targets = enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(filtered))])
        .unwrap()
        .targets;
    assert_eq!(targets.len(), 2);
    for target in &targets {
        assert_eq!(
            target.choices.alternatives,
            alternatives(count()),
            "measure {}",
            target.measure
        );
        assert!(target.choices.pruned.is_empty());
        assert!(target.having.is_empty());
    }
    assert_eq!(targets[0].filter, Some(always()));
    assert_eq!(targets[1].filter, None);
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
/// every candidate.
#[test]
fn key_only_having_is_pushed_below_and_keeps_candidates() {
    let node = grouped_count(None);
    let root = having_filter(node.clone(), equals(0, ScalarValue::Utf8("a".into())));
    let targets = enumerate_local_logical_candidates(vec![(0, QueryRoot::Operator(root.clone()))])
        .unwrap()
        .targets;
    assert_eq!(targets.len(), 1);
    assert!(Rc::ptr_eq(&targets[0].target, &node));
    assert_eq!(
        targets[0].having,
        vec![HavingEntry {
            filter: Some(root.clone()),
            predicate: equals(0, ScalarValue::Utf8("a".into())),
            placement: HavingPlacement::BelowAggregate,
        }]
    );
    assert_eq!(
        targets[0].choices.alternatives,
        alternatives(AggIntent::Count {
            accuracy: approximate()
        })
    );
    assert!(targets[0].choices.pruned.is_empty());
}

/// A `HAVING` over an aggregate value, as a `Filter` above or as the
/// aggregate's own `having`, stays above the output and keeps every candidate.
#[test]
fn value_having_stays_above_output_and_keeps_candidates() {
    let value = || equals(1, ScalarValue::Int64(10));
    let targets = enumerate_local_logical_candidates(vec![
        (
            0,
            QueryRoot::Operator(having_filter(grouped_count(None), value())),
        ),
        (1, QueryRoot::Operator(grouped_count(Some(value())))),
    ])
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 2);
    for target in &targets {
        let placements: Vec<_> = target.having.iter().map(|entry| entry.placement).collect();
        assert_eq!(placements, vec![HavingPlacement::AboveOutput]);
        assert_eq!(
            target.choices.alternatives,
            alternatives(AggIntent::Count {
                accuracy: approximate()
            })
        );
        assert!(target.choices.pruned.is_empty());
    }
}

/// `avg`'s one exact option is sum and count states when its input is never
/// null, and plain `Exact` when that rewrite is pruned.
#[test]
fn avg_rewrites_to_sum_and_count_states() {
    let avg = || vec![AggIntent::Avg { col: None }];
    let targets = enumerate_local_logical_candidates(vec![
        (
            0,
            QueryRoot::Operator(aggregate_over(scan(false), avg(), vec![], None)),
        ),
        (
            1,
            QueryRoot::Operator(aggregate_over(scan(true), avg(), vec![], None)),
        ),
    ])
    .unwrap()
    .targets;
    assert_eq!(
        targets[0].choices.alternatives,
        vec![LocalCandidate::AvgAsSumCount]
    );
    assert_eq!(targets[1].choices.alternatives, vec![LocalCandidate::Exact]);
    assert_eq!(
        targets[1].choices.pruned,
        vec![(LocalCandidate::AvgAsSumCount, PruneReason::NullableAvgInput)]
    );
}

/// A key-only `HAVING` above a shared aggregate stays above the output when
/// another consumer reads the aggregate without it.
#[test]
fn having_on_one_consumer_of_a_shared_aggregate_stays_above() {
    let node = grouped_count(None);
    let key = equals(0, ScalarValue::Utf8("a".into()));
    let filtered = having_filter(node.clone(), key.clone());
    let targets = enumerate_local_logical_candidates(vec![
        (0, QueryRoot::Operator(filtered.clone())),
        (1, QueryRoot::Operator(node.clone())),
    ])
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].having,
        vec![HavingEntry {
            filter: Some(filtered),
            predicate: key.clone(),
            placement: HavingPlacement::AboveOutput,
        }]
    );

    // Two consumers applying the same key-only predicate push it below.
    let first = having_filter(node.clone(), key.clone());
    let second = having_filter(node, key);
    let targets = enumerate_local_logical_candidates(vec![
        (0, QueryRoot::Operator(first)),
        (1, QueryRoot::Operator(second)),
    ])
    .unwrap()
    .targets;
    assert!(targets[0]
        .having
        .iter()
        .all(|entry| entry.placement == HavingPlacement::BelowAggregate));
    assert_eq!(targets[0].having.len(), 2);
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
    let targets = enumerate_local_logical_candidates(vec![
        (
            0,
            QueryRoot::Operator(having_filter(
                aggregate_over(scan(false), count(), vec![], None),
                never.clone(),
            )),
        ),
        (
            1,
            QueryRoot::Operator(aggregate_over(scan(false), count(), vec![], Some(never))),
        ),
    ])
    .unwrap()
    .targets;
    assert_eq!(targets.len(), 2);
    for target in &targets {
        let placements: Vec<_> = target.having.iter().map(|entry| entry.placement).collect();
        assert_eq!(placements, vec![HavingPlacement::AboveOutput]);
    }
}

/// #509 Example 1 Q2 under ImplicitExact: the top-k's one option is plain
/// `Exact`, its heap summaries pruned `RequiresExact`; `sum_over_time` gets
/// its exact Sum accumulator.
#[test]
fn example_1_exact_q2_prunes_topk_heaps() {
    let roots = implicit_exact_promql("topk by (job) (10, sum_over_time(http_requests_total[1m]))");
    let candidates =
        enumerate_local_logical_candidates(roots.into_iter().enumerate().collect()).unwrap();
    let found: Vec<_> = candidates
        .targets
        .iter()
        .map(|target| target.choices.clone())
        .collect();
    let topk = found
        .iter()
        .find(|choices| choices.alternatives == [LocalCandidate::Exact])
        .expect("top-k target");
    assert_eq!(
        topk.pruned,
        vec![
            (
                LocalCandidate::Summary(SketchAlgorithm::CmsWithHeap),
                PruneReason::RequiresExact
            ),
            (
                LocalCandidate::Summary(SketchAlgorithm::CountSketchWithHeap),
                PruneReason::RequiresExact
            ),
        ]
    );
    assert!(found.iter().any(|choices| choices.alternatives
        == [LocalCandidate::ExactAccumulator {
            kind: ExactKind::Sum,
            params: ExactParams::Sum
        }]
        && choices.pruned.is_empty()));
    assert_eq!(found.len(), 2, "{found:#?}");
}
