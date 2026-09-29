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
    let dag = compile_post_asap_dag(&plan).unwrap();
    let build=dag.nodes.iter().find(|node|matches!(&node.payload,PostAsapOperatorPayload::SummaryAgg{family:SummaryFamilyType::Sketch(kind,_),..}if kind.algorithm()==&algorithm)).unwrap();
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
    check_direct_rate_topk(false);
}

// Unreferenced labels still distinguish series throughout Rate and heap readout.
#[test]
fn direct_rate_topk_preserves_dynamic_unreferenced_labels() {
    check_direct_rate_topk(true);
}

fn check_direct_rate_topk(dynamic: bool) {
    use asap_physical_operators::physical_planner::promql_rows::{
        decode_series_identity, series_row, with_series_identity, SERIES_IDENTITY_COLUMN,
    };
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
    if dynamic {
        logical = with_series_identity(&logical).unwrap();
    } else {
        resolve_catalog(&mut logical);
    }
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
        if dynamic {
            let (source, ranked) =
                asap_physical_operators::physical_planner::promql_rows::compile_rate_ranking(
                    candidate,
                )
                .unwrap();
            assert!(matches!(
                source.expr,
                SummaryExpr::ValueOperation {
                    operation: ValueOperation::FinalizeExactAccumulator,
                    ..
                }
            ));
            assert_eq!(ranked.input_contracts().count(), 1);
            let encoded = String::from_utf8(serde_json::to_vec(&ranked).unwrap()).unwrap();
            assert!(encoded.contains("KeyedSummaryBuild"));
            assert!(encoded.contains("KeyedReadout"));
            assert!(
                !encoded.contains("\"Rate\""),
                "Rate must be supplied by its exact stored-state readout"
            );
        }
        let dag = compile_post_asap_dag(candidate).unwrap();
        assert!(dag.nodes.iter().any(|node| matches!(&node.payload,
            PostAsapOperatorPayload::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. } if kind.algorithm() == &algorithm)));
        let build = dag.nodes.iter().find(|node| matches!(&node.payload,
            PostAsapOperatorPayload::SummaryAgg { family: SummaryFamilyType::Sketch(kind, _), .. } if kind.algorithm() == &algorithm)).unwrap();
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
                    PostAsapOperatorPayload::Fallback {
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
        let bytes = serde_json::to_vec(&raw_compiled).unwrap();
        let raw_compiled = serde_json::from_slice::<
            asap_physical_operators::physical_planner::CompiledPhysicalDag,
        >(&bytes)
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
                    if dynamic {
                        raw_rows.push(
                            series_row(
                                &raw_schema,
                                &BTreeMap::from([
                                    ("job".into(), "api".into()),
                                    ("service".into(), service.into()),
                                    ("unreferenced".into(), format!("{service}-extra")),
                                ]),
                                end - 60_000 + offset,
                                value,
                            )
                            .unwrap(),
                        );
                        continue;
                    }
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
                        let batch = batch.unwrap();
                        for row in batch.rows() {
                            if dynamic {
                                let column = batch
                                    .schema()
                                    .fields
                                    .iter()
                                    .position(|field| field.name == SERIES_IDENTITY_COLUMN)
                                    .unwrap();
                                let Value::Utf8(encoded) = &row[column] else {
                                    panic!("identity lost");
                                };
                                let labels = decode_series_identity(encoded).unwrap();
                                assert_eq!(labels["job"], "api");
                                assert_eq!(
                                    labels["unreferenced"],
                                    format!("{}-extra", labels["service"])
                                );
                            }
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
                    if dynamic {
                        return series_row(
                            &schema,
                            &BTreeMap::from([
                                ("job".into(), "api".into()),
                                ("service".into(), service.into()),
                            ]),
                            time,
                            value,
                        )
                        .unwrap();
                    }
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

// Spatial ranking consumes one eligible instant vector. Signed values require
// CountSketch; a raw metric does not establish the non-negative CMS contract.
#[test]
fn spatial_topk_exposes_signed_heap_candidate_over_complete_snapshot() {
    use asap_physical_operators::physical_planner::promql_rows::{
        decode_series_identity, series_row, with_series_identity, SERIES_IDENTITY_COLUMN,
    };
    let logical = lower_promql("topk by(job)(1, m)", AccuracyTarget::Epsilon(0.1)).unwrap();
    let root = Rc::new(with_series_identity(&logical).unwrap());
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &Evidence,
    );
    let candidates = strategy
        .current_series_topk_candidates(&root, &AccuracyTarget::Epsilon(0.1))
        .candidates;
    assert!(!candidates
        .iter()
        .any(|c| c.rationale.contains("CmsWithHeap")));
    let selected = candidates
        .iter()
        .find_map(|candidate| match &candidate.replacement {
            Replacement::Summary(node) if candidate.rationale.contains("CountSketchWithHeap") => {
                Some(node)
            }
            _ => None,
        })
        .expect("signed spatial TopK must expose CountSketch with heap");
    let dag = compile_post_asap_dag(selected).unwrap();
    let raw = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                &node.payload,
                PostAsapOperatorPayload::Fallback {
                    expression: QueryExpr::TimeRange { .. }
                }
            )
        })
        .unwrap();
    let schema = Arc::new(raw.output_schema.clone());
    let program = compile(
        &dag,
        BTreeMap::from([(u64::from(raw.id.0), InputContract::bounded(schema.clone()))]),
        &[u64::from(dag.root.0)],
    )
    .unwrap();
    let snapshot_program =
        asap_physical_operators::physical_planner::promql_rows::compile_current_series_readout(
            selected,
        )
        .unwrap();
    let encoded: serde_json::Value =
        serde_json::from_slice(&serde_json::to_vec(&snapshot_program).unwrap()).unwrap();
    assert!(!encoded.to_string().contains("CurrentSeries"));
    assert!(encoded.to_string().contains("KeyedSummaryBuild"));
    assert!(encoded.to_string().contains("KeyedReadout"));
    for (values, expected, score) in [
        ([100., 20.], "a", 100.),
        ([1., 20.], "b", 20.),
        ([-10., -2.], "b", -2.),
    ] {
        let rows = ["a", "b"]
            .into_iter()
            .zip(values)
            .map(|(instance, value)| {
                series_row(
                    &schema,
                    &BTreeMap::from([
                        ("job".into(), "api".into()),
                        ("unreferenced".into(), instance.into()),
                    ]),
                    60_000,
                    value,
                )
                .unwrap()
            })
            .collect();
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        let graph = program
            .instantiate(BTreeMap::from([(
                u64::from(raw.id.0),
                Box::new(Operator::source(schema.clone(), vec![batch]).unwrap()) as Source<'_>,
            )]))
            .unwrap();
        block_on(async {
            let context = RunContext::new(
                Scope::Query {
                    evaluation_time_ms: 60_000,
                    revision: 0,
                },
                Limits::default(),
            )
            .unwrap();
            let mut stream = graph.execute(program.roots(), context).unwrap().remove(0);
            let mut result = Vec::new();
            while let Some(batch) = stream.next().await {
                let batch = batch.unwrap();
                let identity = batch
                    .schema()
                    .fields
                    .iter()
                    .position(|f| f.name == SERIES_IDENTITY_COLUMN)
                    .unwrap();
                let value = batch
                    .schema()
                    .fields
                    .iter()
                    .position(|f| f.name == "value")
                    .unwrap();
                for row in batch.rows() {
                    let Value::Utf8(labels) = &row[identity] else {
                        panic!()
                    };
                    let Value::Float64(v) = row[value] else {
                        panic!()
                    };
                    result.push((
                        decode_series_identity(labels).unwrap()["unreferenced"].clone(),
                        v,
                    ));
                }
            }
            assert_eq!(result, vec![(expected.into(), score)]);
        });
    }
}

// Placement changes execution ownership only. Every fixed-window candidate
// contains Rate finalization before a fresh heap, with query readout downstream.
#[test]
fn planner_exposes_fixed_window_rate_heap_precompute_candidates() {
    use asap_physical_operators::physical_planner::{
        compile_candidate, promql_rows::with_series_identity,
    };
    let root = Rc::new(
        with_series_identity(
            &lower_promql("topk by(job)(2, rate(m[1m]))", AccuracyTarget::Epsilon(0.1)).unwrap(),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &Evidence,
    );
    let candidates = strategy.fixed_window_rate_candidates(&root).candidates;
    assert_eq!(candidates.len(), 2);
    for candidate in candidates {
        let Replacement::Summary(root) = candidate.replacement else {
            panic!()
        };
        let dag = compile_post_asap_dag(&root).unwrap();
        let state = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.payload,
                    PostAsapOperatorPayload::SummaryAgg {
                        family: SummaryFamilyType::ExactAggregate(ExactKind::Rate, _),
                        ..
                    }
                )
            })
            .unwrap();
        let heap = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.payload,
                    PostAsapOperatorPayload::SummaryAgg {
                        family: SummaryFamilyType::Sketch(..),
                        ..
                    }
                )
            })
            .unwrap();
        assert_eq!(heap.output_state.timing, ExecutionTiming::IngestionTime);
        let physical = compile_candidate(
            &dag,
            BTreeMap::from([(
                u64::from(state.id.0),
                InputContract::bounded(Arc::new(state.output_schema.clone())),
            )]),
            &[u64::from(dag.root.0)],
            &[u64::from(heap.id.0)],
        )
        .unwrap();
        let exported = asap_physical_operators::physical_planner::promql_rows::compile_fixed_window_rate_aggregation(&root).unwrap();
        assert_eq!(
            serde_json::to_vec(&exported).unwrap(),
            serde_json::to_vec(&physical).unwrap()
        );
        assert!(
            asap_physical_operators::physical_planner::promql_rows::compile_rate_ranking(&root)
                .is_err(),
            "query binding must not move the selected precompute frontier"
        );
        // Execute the selected split across a state serialization boundary.
        // Each run builds fresh weights from that window's counters.
        let execute = |plan: &asap_physical_operators::physical_planner::CompiledPhysicalDag,
                       input: Batch,
                       scope: Scope| {
            let id = plan.input_contracts().next().unwrap().0;
            let source = Box::new(Operator::source(input.schema().clone(), vec![input]).unwrap())
                as Source<'static>;
            let graph = plan.instantiate(BTreeMap::from([(id, source)])).unwrap();
            block_on(async {
                let mut stream = graph
                    .execute(
                        plan.roots(),
                        RunContext::new(scope, Limits::default()).unwrap(),
                    )
                    .unwrap()
                    .remove(0);
                let mut batches = Vec::new();
                while let Some(batch) = stream.next().await {
                    batches.push((*batch.unwrap()).clone());
                }
                assert_eq!(batches.len(), 1);
                batches.remove(0)
            })
        };
        let (family, input, grouping) = match &state.payload {
            PostAsapOperatorPayload::SummaryAgg {
                family,
                input,
                grouping,
                ..
            } => (family, input, grouping),
            _ => unreachable!(),
        };
        for (end, samples, leader) in [
            (
                60_000,
                [[0., 100., 200.], [0., 10., 20.], [0., 1., 2.]],
                "a",
            ),
            (
                120_000,
                [[200., 200., 200.], [100., 0., 300.], [2., 3., 4.]],
                "b",
            ),
        ] {
            let schema = Arc::new(state.output_schema.clone());
            let rows = samples
                .into_iter()
                .zip(["a", "b", "c"])
                .map(|(samples, label)| {
                    let mut accumulator =
                        asap_physical_operators::factory::create_planner_accumulator(
                            family, input, grouping,
                        )
                        .unwrap();
                    for (offset, value) in [10_000, 30_000, 50_000].into_iter().zip(samples) {
                        accumulator.update_single(value, end - 60_000 + offset);
                    }
                    let summary = Value::Summary {
                        family: family.clone(),
                        state: Arc::from(accumulator.into_accumulator()),
                    };
                    schema
                        .fields
                        .iter()
                        .map(|field| match &field.dtype {
                            SummaryFamilyType::ExactAggregate(..) => summary.clone(),
                            SummaryFamilyType::Plain(DataType::Timestamp) => Value::Timestamp(end),
                            SummaryFamilyType::Plain(DataType::Utf8)
                                if field.name == "$promql_series_identity" =>
                            {
                                Value::Utf8(
                                    serde_json::to_string(&BTreeMap::from([
                                        ("job", "api"),
                                        ("instance", label),
                                    ]))
                                    .unwrap()
                                    .into(),
                                )
                            }
                            SummaryFamilyType::Plain(DataType::Utf8) => Value::Utf8("api".into()),
                            _ => panic!("unexpected state field {field:?}"),
                        })
                        .collect()
                })
                .collect();
            let batch = Batch::try_new(schema, rows).unwrap();
            let precompute = physical.precompute.as_ref().unwrap();
            let heap = execute(
                precompute,
                batch,
                Scope::Ingestion {
                    window_start_ms: end - 60_000,
                    window_end_ms: end,
                    revision: 1,
                },
            );
            let result = execute(
                &physical.query,
                heap,
                Scope::Query {
                    evaluation_time_ms: end,
                    revision: 1,
                },
            );
            let identity = result
                .schema()
                .fields
                .iter()
                .position(|f| f.name == "$promql_series_identity")
                .unwrap();
            let Value::Utf8(encoded) = &result.rows()[0][identity] else {
                panic!()
            };
            let labels: BTreeMap<String, String> = serde_json::from_str(encoded).unwrap();
            assert_eq!(labels["instance"], leader);
            assert_eq!(result.rows().len(), 2);
        }
        let precompute =
            String::from_utf8(serde_json::to_vec(&physical.precompute.unwrap()).unwrap()).unwrap();
        assert!(precompute.contains("KeyedSummaryBuild"));
        assert!(precompute.contains("Rate"));
        assert!(
            !String::from_utf8(serde_json::to_vec(&physical.query).unwrap())
                .unwrap()
                .contains("KeyedSummaryBuild")
        );
    }
}

// Grouped Rate has a legal stored Sum candidate as well as query-time reduction.
#[test]
fn grouped_rate_exposes_precomputed_sum_with_query_readout() {
    let root = Rc::new(
        asap_physical_operators::physical_planner::promql_rows::with_series_identity(
            &lower_promql("sum by(job)(rate(m[1m]))", AccuracyTarget::Exact).unwrap(),
        )
        .unwrap(),
    );
    let strategy = SketchAlgorithmStrategy::new_with_planning_inputs_and_evidence(
        &DefaultCostModel,
        &DefaultAccuracyModel,
        &EqualSplitAllocator,
        &Evidence,
    );
    let direct = strategy.query_time_rate_aggregation_candidates(&root);
    assert!(
        direct.candidates.iter().any(|candidate| {
            let Replacement::Summary(root) = &candidate.replacement else {
                return false;
            };
            let Ok((_, program)) =
                asap_physical_operators::physical_planner::promql_rows::compile_rate_ranking(root)
            else {
                return false;
            };
            let output = program.output_contract(program.roots()[0]).unwrap();
            output
                .schema
                .fields
                .iter()
                .all(|field| matches!(field.dtype, SummaryFamilyType::Plain(_)))
        }),
        "query-time grouped Rate must finalize Sum inside the physical graph"
    );
    let candidates = strategy.fixed_window_rate_candidates(&root).candidates;
    assert!(
        !candidates.is_empty(),
        "Planner must expose Rate -> grouped Sum at ingestion"
    );
    for candidate in candidates {
        let Replacement::Summary(root) = candidate.replacement else {
            panic!()
        };
        let physical = asap_physical_operators::physical_planner::promql_rows::compile_fixed_window_rate_aggregation(&root).unwrap();
        let precompute =
            String::from_utf8(serde_json::to_vec(&physical.precompute.unwrap()).unwrap()).unwrap();
        assert!(
            precompute.contains("SummaryBuild")
                && precompute.contains("Rate")
                && precompute.contains("Sum")
        );
        let query = String::from_utf8(serde_json::to_vec(&physical.query).unwrap()).unwrap();
        assert!(query.contains("Readout") && !query.contains("SummaryBuild"));
    }
}
