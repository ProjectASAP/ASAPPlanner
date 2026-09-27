//! Planner output binds directly to the shared runtime at a declared rate-value frontier.
use asap_aware_mapping::{
    accuracy::{
        AccuracyEvidenceProvider, DefaultAccuracyModel, EqualSplitAllocator, PropagationStats,
    },
    cost_model::DefaultCostModel,
    Replacement, ReplacementStrategy, SketchAlgorithmStrategy, TargetSubDAG,
};
use asap_physical_operators::dag::{
    operators::Operator,
    planner::{compile, InputContract, Source},
    values::{Batch, Value},
    Limits, RunContext, Scope,
};
use futures::{executor::block_on, StreamExt};
use planner_types::{
    post_asap::*,
    pre_asap::{DataType, QueryExpr},
    types::AccuracyTarget,
};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};
struct Evidence;
impl AccuracyEvidenceProvider for Evidence {
    fn topk_max_distinct_items(&self, _: &QueryExpr) -> Option<u64> {
        Some(1000)
    }
    fn propagation_stats(
        &self,
        op: &CompositionOperator,
        _: &SummaryFamilyType,
        _: Option<&SketchQuery>,
    ) -> PropagationStats {
        if matches!(op, CompositionOperator::TopKSelection) {
            PropagationStats {
                topk_selected_lower_bound: Some(101.),
                topk_excluded_upper_bound: Some(100.),
                topk_interval_failure_probability: Some(0.001),
                ..Default::default()
            }
        } else {
            Default::default()
        }
    }
}
// The evidence here exercises binding; it is not inferred from the sample data.
#[test]
fn planner_weighted_topk_binds_at_either_deployment_phase() {
    assert_weighted_binding(&Evidence, SketchAlgorithm::CmsWithHeap);
    assert_weighted_binding(&Evidence, SketchAlgorithm::CountSketchWithHeap);
}

// Binding validates representation, while deployment owns evidence acceptance.
#[test]
fn physical_binding_does_not_impose_an_accuracy_acceptance_policy() {
    assert_weighted_binding(
        &asap_aware_mapping::accuracy::NoAccuracyEvidence,
        SketchAlgorithm::CmsWithHeap,
    );
    assert_weighted_binding(
        &asap_aware_mapping::accuracy::NoAccuracyEvidence,
        SketchAlgorithm::CountSketchWithHeap,
    );
}

fn assert_weighted_binding(evidence: &dyn AccuracyEvidenceProvider, algorithm: SketchAlgorithm) {
    let root = Rc::new(
        lower_promql(
            "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
            AccuracyTarget::Epsilon(0.1),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        evidence,
    );
    let plan = strategy
        .replacements(&TargetSubDAG::new(&root))
        .into_iter()
        .find_map(|candidate| match candidate.replacement {
            Replacement::Summary(node)
                if candidate.rationale.contains(&format!("{algorithm:?}")) =>
            {
                Some(node)
            }
            _ => None,
        })
        .unwrap();
    let dag = compile_executable_dag(&plan).unwrap();
    let build=dag.nodes.iter().find(|node|matches!(&node.payload,ExecutableOperatorPayload::SummaryAgg{family:SummaryFamilyType::Sketch(kind,_),..}if kind.algorithm()==&algorithm)).unwrap();
    let rate_id = dag
        .edges
        .iter()
        .find(|edge| edge.consumer == build.id)
        .unwrap()
        .producer;
    let rates = Arc::new(
        dag.nodes
            .iter()
            .find(|node| node.id == rate_id)
            .unwrap()
            .output_schema
            .clone(),
    );
    let rows = [
        ("auth", "api", 0.125),
        ("auth", "api", 0.25),
        ("checkout", "api", 0.3125),
        ("search", "api", 0.0625),
        ("ingest", "batch", 100.),
        ("export", "batch", 80.),
        ("cleanup", "batch", 20.),
    ]
    .into_iter()
    .map(|(service, job, value)| {
        rates
            .fields
            .iter()
            .map(|field| match field.name.as_str() {
                "service" => Value::Utf8(service.into()),
                "job" => Value::Utf8(job.into()),
                "value" => Value::Float64(value),
                _ => match field.dtype {
                    SummaryFamilyType::Plain(DataType::Timestamp) => Value::Timestamp(60_000),
                    _ => panic!("unexpected rate column {field:?}"),
                },
            })
            .collect()
    })
    .collect();
    let batch = Batch::try_new(rates.clone(), rows).unwrap();
    for (phase, scope) in [
        (
            ExecutionTiming::IngestionTime,
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 60_000,
                revision: 1,
            },
        ),
        (
            ExecutionTiming::QueryTime,
            Scope::Query {
                evaluation_time_ms: 60_000,
                revision: 1,
            },
        ),
    ] {
        let placed = dag
            .with_execution_phases(&dag.nodes.iter().map(|node| (node.id, phase)).collect())
            .unwrap();
        let source = Box::new(Operator::source(rates.clone(), vec![batch.clone()]).unwrap())
            as Source<'static>;
        let compiled = compile(
            &placed,
            BTreeMap::from([(rate_id.0 as u64, InputContract::bounded(rates.clone()))]),
            &[dag.root.0 as u64],
        )
        .unwrap();
        let graph = compiled
            .instantiate(BTreeMap::from([(rate_id.0 as u64, source)]))
            .unwrap();
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let output = block_on(async {
            let mut output = Vec::new();
            let mut stream = graph
                .execute(&[dag.root.0 as u64], context)
                .unwrap()
                .remove(0);
            while let Some(batch) = stream.next().await {
                output.extend(batch.unwrap().rows().iter().cloned());
            }
            output
        });
        assert_eq!(output.len(), 4);
        let mut scores = output
            .iter()
            .map(|row| {
                row.iter()
                    .find_map(|v| {
                        if let Value::Float64(v) = v {
                            Some(*v)
                        } else {
                            None
                        }
                    })
                    .unwrap()
            })
            .collect::<Vec<_>>();
        scores.sort_by(f64::total_cmp);
        assert_eq!(scores, vec![0.3125, 0.375, 80., 100.]);
    }
}

use asap_frontend_promql::lower_promql_workload;
use planner_types::workload::{
    AccuracyRequirement, BatchEntry, DataWorkload, DurationMs, Evidence as WorkloadEvidence,
    PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload,
    TimeSelection,
};
pub fn lower_promql(
    query: &str,
    accuracy: AccuracyTarget,
) -> Result<QueryExpr, asap_frontend_promql::PromqlError> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(accuracy),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload {
            data_ingestion_interval: WorkloadEvidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let mut lowered = lower_promql_workload(&workload, 0)?;
    Ok(lowered.remove(0))
}

// The old untyped heap updater must not silently round a Planner rate update.
#[test]
fn rate_updates_cannot_enter_integer_heap_factory() {
    let family = SummaryFamilyType::Sketch(
        SketchKind::new(
            SketchAlgorithm::CmsWithHeap,
            SketchParams::CmsWithHeap {
                width: 272,
                depth: 5,
                heap_size: 100,
            },
        ),
        Default::default(),
    );
    let input = SummaryUpdate {
        item: Some(SummaryInputExpr::Column(
            planner_types::pre_asap::ColumnRef::Named("service".into()),
        )),
        weight: SummaryInputExpr::Column(planner_types::pre_asap::ColumnRef::SampleValue),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    };
    assert!(
        asap_physical_operators::factory::create_planner_accumulator(
            &family,
            &input,
            &Default::default()
        )
        .is_err()
    );
}

/// A catalog-resolved per-series rate can feed a heap sketch directly, without
/// requiring an otherwise unnecessary grouped Sum between Rate and TopK.
#[test]
fn direct_rate_topk_exposes_heap_candidates_with_complete_series_identity() {
    let mut logical =
        lower_promql("topk by(job)(2, rate(m[1m]))", AccuracyTarget::Epsilon(0.1)).unwrap();
    fn resolve_catalog(node: &mut QueryExpr) {
        match node {
            QueryExpr::Aggregate { child, .. } | QueryExpr::TimeRange { child, .. } => {
                resolve_catalog(Rc::make_mut(child))
            }
            QueryExpr::Scan { schema, .. } => {
                schema.closed = true;
                schema
                    .columns
                    .push(planner_types::pre_asap::schema::Column::new(
                        "service",
                        DataType::Utf8,
                        false,
                    ));
            }
            _ => panic!("unexpected input shape: {node:?}"),
        }
    }
    resolve_catalog(&mut logical);
    let root = Rc::new(logical);
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &Evidence,
    );
    let candidates = strategy.replacements(&TargetSubDAG::new(&root));
    for algorithm in [
        SketchAlgorithm::CmsWithHeap,
        SketchAlgorithm::CountSketchWithHeap,
    ] {
        let candidate = candidates
            .iter()
            .find_map(|candidate| match &candidate.replacement {
                Replacement::Summary(node)
                    if candidate.rationale.contains(&format!("{algorithm:?}")) =>
                {
                    Some(node)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing {algorithm:?} over direct Rate"));
        let dag = compile_executable_dag(candidate).unwrap();
        assert!(dag.nodes.iter().any(|node| matches!(&node.payload,
            ExecutableOperatorPayload::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. } if kind.algorithm() == &algorithm)));
        let build = dag.nodes.iter().find(|node| matches!(&node.payload,
            ExecutableOperatorPayload::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. } if kind.algorithm() == &algorithm)).unwrap();
        let input_id = dag
            .edges
            .iter()
            .find(|edge| edge.consumer == build.id)
            .unwrap()
            .producer;
        let schema = Arc::new(
            dag.nodes
                .iter()
                .find(|node| node.id == input_id)
                .unwrap()
                .output_schema
                .clone(),
        );
        let raw = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.payload,
                    ExecutableOperatorPayload::Fallback {
                        expression: QueryExpr::TimeRange { .. }
                    }
                )
            })
            .unwrap_or_else(|| panic!("no raw counter source: {dag:?}"));
        let raw_schema = Arc::new(raw.output_schema.clone());
        let raw_compiled = compile(
            &dag,
            BTreeMap::from([(
                u64::from(raw.id.0),
                InputContract::bounded(raw_schema.clone()),
            )]),
            &[u64::from(dag.root.0)],
        )
        .unwrap();
        // Each evaluation receives a complete raw window. A reset, a stopped
        // series and an expired leader must not retain last run's heap weights.
        for (end, series, expected) in [
            (
                60_000,
                vec![
                    ("auth", vec![10., 30., 50.]),
                    ("checkout", vec![10., 50., 90.]),
                    ("search", vec![10., 70., 130.]),
                ],
                vec![11. / 6., 8. / 3.],
            ),
            (
                120_000,
                vec![
                    ("auth", vec![100., 10., 50.]),
                    ("checkout", vec![100., 100., 100.]),
                ],
                vec![0., 1.25],
            ),
        ] {
            let mut raw_rows = Vec::new();
            for (service, samples) in series {
                for (offset, value) in [10_000, 30_000, 50_000].into_iter().zip(samples) {
                    raw_rows.push(
                        raw_schema
                            .fields
                            .iter()
                            .map(|field| match field.name.as_str() {
                                "service" => Value::Utf8(service.into()),
                                "job" => Value::Utf8("api".into()),
                                "value" => Value::Float64(value),
                                "ts" => Value::Timestamp(end - 60_000 + offset),
                                _ => panic!("unexpected raw field"),
                            })
                            .collect(),
                    );
                }
            }
            let raw_batch = Batch::try_new(raw_schema.clone(), raw_rows).unwrap();
            for scope in [
                Scope::Ingestion {
                    window_start_ms: end - 60_000,
                    window_end_ms: end,
                    revision: 1,
                },
                Scope::Query {
                    evaluation_time_ms: end,
                    revision: 1,
                },
            ] {
                let source = Box::new(
                    Operator::source(raw_schema.clone(), vec![raw_batch.clone()]).unwrap(),
                ) as Source<'static>;
                let graph = raw_compiled
                    .instantiate(BTreeMap::from([(u64::from(raw.id.0), source)]))
                    .unwrap();
                let context = RunContext::new(scope, Limits::default()).unwrap();
                let mut raw_scores = block_on(async {
                    let mut scores = Vec::new();
                    let mut stream = graph
                        .execute(&[u64::from(dag.root.0)], context)
                        .unwrap()
                        .remove(0);
                    while let Some(batch) = stream.next().await {
                        for row in batch.unwrap().rows() {
                            assert!(row.iter().any(
                                |value| matches!(value, Value::Timestamp(time) if *time == end)
                            ));
                            scores.extend(row.iter().filter_map(|value| match value {
                                Value::Float64(value) => Some(*value),
                                _ => None,
                            }));
                        }
                    }
                    scores
                });
                raw_scores.sort_by(f64::total_cmp);
                assert_eq!(raw_scores.len(), expected.len());
                for (actual, expected) in raw_scores.iter().zip(&expected) {
                    assert!(
                        (actual - expected).abs() < 1e-12,
                        "raw counter semantics must precede heap ranking: {raw_scores:?}"
                    );
                }
            }
        }
        let compiled = compile(
            &dag,
            BTreeMap::from([(
                u64::from(input_id.0),
                InputContract::bounded(schema.clone()),
            )]),
            &[u64::from(dag.root.0)],
        )
        .unwrap();
        for (time, values, expected) in [
            (
                60_000,
                vec![("auth", 3.), ("checkout", 2.), ("search", 1.)],
                vec![2., 3.],
            ),
            (
                61_000,
                vec![("auth", 0.), ("checkout", 2.), ("search", 4.)],
                vec![2., 4.],
            ),
            (62_000, vec![("auth", 0.), ("checkout", 2.)], vec![0., 2.]),
        ] {
            let rows = values
                .into_iter()
                .map(|(service, value)| {
                    schema
                        .fields
                        .iter()
                        .map(|field| match field.name.as_str() {
                            "service" => Value::Utf8(service.into()),
                            "job" => Value::Utf8("api".into()),
                            "value" => Value::Float64(value),
                            "ts" => Value::Timestamp(time),
                            _ => panic!("unexpected rate field {field:?}"),
                        })
                        .collect()
                })
                .collect();
            let batch = Batch::try_new(schema.clone(), rows).unwrap();
            for scope in [
                Scope::Query {
                    evaluation_time_ms: time,
                    revision: 1,
                },
                Scope::Ingestion {
                    window_start_ms: time - 60_000,
                    window_end_ms: time,
                    revision: 1,
                },
            ] {
                let source =
                    Box::new(Operator::source(schema.clone(), vec![batch.clone()]).unwrap())
                        as Source<'static>;
                let graph = compiled
                    .instantiate(BTreeMap::from([(u64::from(input_id.0), source)]))
                    .unwrap();
                let context = RunContext::new(scope, Limits::default()).unwrap();
                let mut scores = block_on(async {
                    let mut scores = vec![];
                    let mut stream = graph
                        .execute(&[u64::from(dag.root.0)], context)
                        .unwrap()
                        .remove(0);
                    while let Some(batch) = stream.next().await {
                        let batch = batch.unwrap();
                        for row in batch.rows() {
                            assert!(row.iter().any(
                                |value| matches!(value, Value::Timestamp(actual) if *actual == time)
                            ));
                            scores.push(
                                row.iter()
                                    .find_map(|value| {
                                        if let Value::Float64(value) = value {
                                            Some(*value)
                                        } else {
                                            None
                                        }
                                    })
                                    .unwrap(),
                            );
                        }
                    }
                    scores
                });
                scores.sort_by(f64::total_cmp);
                assert_eq!(
                    scores, expected,
                    "heap snapshots must not accumulate across evaluations"
                );
            }
        }
    }
}
