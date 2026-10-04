//! Frontend-to-Pass-1 acceptance: candidate discovery precedes empirical selection.
use asap_logical_optimizer::{
    pass1::logical_candidates::enumerate_local_logical_candidates,
    pass1::logical_candidates::local_realizations_for_intent,
    pass1::logical_candidates::LogicalCandidateError, Realization,
};
use asap_types::ir::operator::operator_properties::{Reduction, Source};
use asap_types::ir::operator::AggIntent;
use asap_types::ir::schema::{DataType, ExactKind, Field, Schema, SketchAlgorithm};
use asap_types::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::types::AccuracyTarget;
use std::rc::Rc;

fn approximate() -> AccuracyTarget {
    AccuracyTarget::EpsilonDelta {
        epsilon: 0.05,
        delta: 0.01,
    }
}
fn algorithms(choices: &[Realization]) -> Vec<SketchAlgorithm> {
    choices
        .iter()
        .filter_map(|choice| match choice {
            Realization::Sketch(kind) => Some(kind.algorithm().clone()),
            _ => None,
        })
        .collect()
}
fn aggregate(intent: AggIntent) -> Rc<OperatorNode> {
    let child = OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Scan {
        source: Source::Table {
            table_ref: "flows".into(),
        },
        predicates: vec![],
        schema: Schema::lifted(vec![Field::plain("src_ip", DataType::Utf8, false)], None),
    }))
    .unwrap();
    OperatorNode::new_shared(Operator::NonASAP(NonASAPOp::Aggregate {
        child,
        reduction: Reduction::by(vec![]),
        measures: vec![intent],
        output_names: vec![],
        filters: vec![],
        having: None,
    }))
    .unwrap()
}

/// Example 2 preserves specialized distinct summaries and the universal alternative.
#[test]
fn cardinality_keeps_exact_specialized_and_universal_alternatives() {
    let choices = local_realizations_for_intent(&AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    })
    .unwrap();
    assert!(matches!(choices[0], Realization::PassThrough));
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::Hll,
            SketchAlgorithm::Theta,
            SketchAlgorithm::Kmv,
            SketchAlgorithm::UnivMon
        ]
    );
    let tuple = local_realizations_for_intent(&AggIntent::Cardinality {
        cols: vec![0, 1],
        accuracy: approximate(),
    })
    .unwrap();
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
        let choices = local_realizations_for_intent(&intent).unwrap();
        assert!(matches!(choices[0], Realization::PassThrough));
        assert_eq!(algorithms(&choices), vec![SketchAlgorithm::UnivMon]);
    }
}

/// An exact request cannot acquire an approximate sketch merely because one is available.
#[test]
fn exact_quantile_stays_exact_and_approximate_keeps_both_families() {
    let choices = local_realizations_for_intent(&AggIntent::Quantile {
        col: Some(0),
        q: 0.99,
        accuracy: approximate(),
    })
    .unwrap();
    assert_eq!(
        algorithms(&choices),
        vec![SketchAlgorithm::Kll, SketchAlgorithm::DDSketch]
    );
    let exact = local_realizations_for_intent(&AggIntent::Quantile {
        col: Some(0),
        q: 0.99,
        accuracy: AccuracyTarget::Exact,
    })
    .unwrap();
    assert_eq!(exact, vec![Realization::PassThrough]);
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
    let candidates = enumerate_local_logical_candidates(roots, &Default::default()).unwrap();
    assert_eq!(candidates.roots.len(), 2);
    for (_, root) in &candidates.roots {
        asap_types::ir::export::compile_logical_asap_query(root)
            .unwrap()
            .validate()
            .unwrap();
    }
    assert_eq!(candidates.targets.len(), 1);
    assert!(Rc::ptr_eq(&candidates.targets[0].target, &producer));
    assert!(producer.timing.is_none());
    assert!(producer.guarantee.is_none());
    asap_types::ir::export::compile_logical_asap_dag(&producer)
        .unwrap()
        .validate()
        .unwrap();
}

/// Example 1 rate lowering reaches the exact accumulator choice without a cost model.
#[test]
fn promql_lowering_reaches_local_candidates_without_execution_timing() {
    use asap_types::workload::{
        AccuracyRequirement, BatchEntry, PlanningWorkload, Query, QueryLanguage, QueryRequirements,
        QueryWorkload,
    };
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query("sum by (job) (rate(http_requests_total[1m]))".into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(approximate()),
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
    let roots = asap_frontend_promql::lower_promql_query_workload(&workload, 0).unwrap();
    let candidates = enumerate_local_logical_candidates(
        roots.into_iter().enumerate().collect(),
        &Default::default(),
    )
    .unwrap();
    assert!(candidates
        .targets
        .iter()
        .any(|target| target.alternatives.iter().any(|choice| matches!(
            choice,
            Realization::ExactAggregate {
                kind: ExactKind::Rate,
                ..
            }
        ))));
    assert!(candidates
        .targets
        .iter()
        .all(|target| target.target.timing.is_none()));
}

/// Physical annotations and invalid probability requirements fail at the stage boundary.
#[test]
fn assigned_timing_and_invalid_accuracy_are_rejected() {
    let mut producer = (*aggregate(AggIntent::Count {
        accuracy: approximate(),
    }))
    .clone();
    producer.timing = Some(asap_types::ir::properties::ExecutionTiming::QueryTime);
    assert!(matches!(
        enumerate_local_logical_candidates(
            vec![(0, QueryRoot::Operator(Rc::new(producer)))],
            &Default::default()
        ),
        Err(LogicalCandidateError::AssignedTiming)
    ));
    for target in [
        AccuracyTarget::Epsilon(f64::NAN),
        AccuracyTarget::EpsilonDelta {
            epsilon: 0.1,
            delta: 0.0,
        },
    ] {
        assert!(matches!(
            local_realizations_for_intent(&AggIntent::Count { accuracy: target }),
            Err(LogicalCandidateError::InvalidAccuracy)
        ));
    }
}

/// Local TopK keeps both declared heap substrates without choosing an implementation.
#[test]
fn topk_keeps_both_specialized_heap_choices() {
    let choices = local_realizations_for_intent(&AggIntent::TopK {
        k: 10,
        accuracy: approximate(),
    })
    .unwrap();
    assert!(matches!(choices[0], Realization::PassThrough));
    assert_eq!(
        algorithms(&choices),
        vec![
            SketchAlgorithm::CmsWithHeap,
            SketchAlgorithm::CountSketchWithHeap
        ]
    );
}

/// A workload candidate replaces only chosen targets, declares whole-source
/// coverage on the summary, and keeps unchosen plans identical.
#[test]
fn composed_candidate_replaces_chosen_target_with_summary_evaluation() {
    use asap_logical_optimizer::pass1::logical_candidates::compose_logical_candidate;
    use asap_types::ir::ASAPOp;
    let producer = aggregate(AggIntent::Cardinality {
        cols: vec![0],
        accuracy: approximate(),
    });
    let inventory = enumerate_local_logical_candidates(
        vec![(0, QueryRoot::Operator(producer.clone()))],
        &Default::default(),
    )
    .unwrap();
    let exact = compose_logical_candidate(&inventory, &[0]).unwrap();
    assert!(matches!(&exact[0].1, QueryRoot::Operator(node) if Rc::ptr_eq(node, &producer)));

    let hll = compose_logical_candidate(&inventory, &[1]).unwrap();
    let QueryRoot::Operator(estimate) = &hll[0].1 else {
        panic!("operator root expected")
    };
    let Some(ASAPOp::SummaryEstimate { summary_input, .. }) = estimate.asap() else {
        panic!("summary evaluation expected")
    };
    let coverage = summary_input.coverage.as_ref().unwrap();
    assert_eq!(
        coverage.source,
        Source::Table {
            table_ref: "flows".into()
        }
    );
    assert!(coverage.regions[0].time_ms.is_none() && coverage.regions[0].population.is_empty());
    asap_types::ir::export::compile_logical_asap_workload(&[hll[0].1.clone()])
        .unwrap()
        .validate()
        .unwrap();
    assert!(compose_logical_candidate(&inventory, &[99]).is_err());
}
