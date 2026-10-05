//! Planner output binds directly to the shared runtime at a declared rate-value frontier.
mod common;
use asap_executor::dag::{
    operators::Operator,
    planner::{compile, InputContract, Source},
    values::{Batch, Value},
    Limits, RunContext, Scope,
};
use common::{compile_physical_asap_dag, stage1_candidates};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use planner_types::ir::properties::*;
use planner_types::ir::schema::{DataType, *};
use planner_types::types::AccuracyTarget;
use std::{collections::BTreeMap, rc::Rc, sync::Arc};
#[test]
fn planner_weighted_topk_binds_at_either_deployment_phase() {
    assert_weighted_binding(SketchAlgorithm::CmsWithHeap);
    assert_weighted_binding(SketchAlgorithm::CountSketchWithHeap);
}

/// Whether `dag` builds a heap sketch of `algorithm`.
fn builds(dag: &PhysicalASAPDAG, algorithm: &SketchAlgorithm) -> bool {
    dag.nodes.iter().any(|node| matches!(&node.payload,
        PhysicalASAPOperatorPayload::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. } if kind.algorithm() == algorithm))
}

fn assert_weighted_binding(algorithm: SketchAlgorithm) {
    let root = lower_promql(
        "topk by(job)(2, sum by(service, job)(rate(m[1m])))",
        AccuracyTarget::Epsilon(0.1),
    )
    .unwrap();
    // The whole-expression heap: it ranks the rates, absorbing the grouped Sum.
    let dag = stage1_candidates(&root)
        .iter()
        .map(|candidate| compile_physical_asap_dag(candidate).unwrap())
        .find(|dag| builds(dag, &algorithm))
        .unwrap();
    let build=dag.nodes.iter().find(|node|matches!(&node.payload,PhysicalASAPOperatorPayload::SummaryAgg{family:FieldDataType::Sketch(kind,_),..}if kind.algorithm()==&algorithm)).unwrap();
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
                    FieldDataType::Plain(DataType::Timestamp) => Value::Timestamp(60_000),
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
            &[dag.roots[0].0 as u64],
        )
        .unwrap();
        let physical_dag = compiled
            .instantiate(BTreeMap::from([(rate_id.0 as u64, source)]))
            .unwrap();
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let output = block_on(async {
            let mut output = Vec::new();
            let mut stream = physical_dag
                .execute(&[dag.roots[0].0 as u64], context)
                .unwrap()
                .remove(0);
            while let Some(batch) = stream.next().await {
                output.extend(batch.unwrap().rows().iter().cloned());
            }
            output
        });
        // Two items per `job`: the outer `by(job)` partitions the ranking.
        let schema = &dag
            .nodes
            .iter()
            .find(|node| node.id == dag.roots[0])
            .unwrap()
            .output_schema;
        let column = |name: &str| schema.fields.iter().position(|f| f.name == name).unwrap();
        let (job, value) = (column("job"), column("value"));
        let mut ranked = output
            .iter()
            .map(|row| match (&row[job], &row[value]) {
                (Value::Utf8(job), Value::Float64(score)) => (job.to_string(), *score),
                other => panic!("unexpected (job, value) {other:?}"),
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
        let expected = [
            ("api", 0.3125),
            ("api", 0.375),
            ("batch", 80.),
            ("batch", 100.),
        ];
        assert_eq!(
            ranked,
            expected.map(|(job, score)| (job.to_string(), score)),
            "{phase:?}"
        );
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
) -> Result<Rc<planner_types::ir::OperatorNode>, asap_frontend_promql::PromqlError> {
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
    let family = FieldDataType::Sketch(
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
            planner_types::ir::scalar::ColumnRef::Named("service".into()),
        )),
        weight: SummaryInputExpr::Column(planner_types::ir::scalar::ColumnRef::SampleValue),
        weight_domain: WeightDomain::NonNegative {
            proof: NonNegativeWeightProof::ResetAwareCounterDerivative,
        },
    };
    assert!(asap_executor::factory::create_planner_accumulator(
        &family,
        &input,
        &Default::default()
    )
    .is_err());
}

// A per-series rate feeds a heap sketch directly, without a grouped Sum
// between Rate and TopK. Unreferenced labels still distinguish series
// throughout Rate and heap evaluation.
#[test]
fn direct_rate_topk_preserves_dynamic_unreferenced_labels() {
    use asap_executor::physical_planner::promql_rows::{
        decode_series_identity, series_row, with_series_identity, SERIES_IDENTITY_COLUMN,
    };
    let logical =
        lower_promql("topk by(job)(2, rate(m[1m]))", AccuracyTarget::Epsilon(0.1)).unwrap();
    let root = with_series_identity(&logical).unwrap();
    let candidates = stage1_candidates(&root);
    // Stage 1 proves no sign for rate values, so a Count-Min heap over them
    // is not executable; only Count Sketch is.
    let algorithm = SketchAlgorithm::CountSketchWithHeap;
    // The heap over the Rate realized as an exact accumulator.
    let candidate = candidates
        .iter()
        .find(|candidate| {
            let dag = compile_physical_asap_dag(candidate).unwrap();
            builds(&dag, &algorithm)
                && dag.nodes.iter().any(|node| {
                    matches!(
                        &node.payload,
                        PhysicalASAPOperatorPayload::SummaryAgg {
                            family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
                            ..
                        }
                    )
                })
        })
        .unwrap_or_else(|| panic!("missing {algorithm:?} over direct Rate"));
    let (source, ranked) =
        asap_executor::physical_planner::promql_rows::compile_rate_ranking(candidate).unwrap();
    assert!(matches!(
        source.operator,
        planner_types::ir::Operator::ASAP(
            planner_types::ir::ASAPOp::FinalizeExactAccumulator { .. }
        )
    ));
    assert_eq!(ranked.input_contracts().count(), 1);
    let encoded = String::from_utf8(serde_json::to_vec(&ranked).unwrap()).unwrap();
    assert!(encoded.contains("KeyedSummaryBuild"));
    assert!(encoded.contains("KeyedEvaluation"));
    assert!(
        !encoded.contains("\"Rate\""),
        "Rate must be supplied by its exact stored-state evaluation"
    );
    let dag = compile_physical_asap_dag(candidate).unwrap();
    assert!(dag.nodes.iter().any(|node| matches!(&node.payload,
        PhysicalASAPOperatorPayload::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. } if kind.algorithm() == &algorithm)));
    let build = dag.nodes.iter().find(|node| matches!(&node.payload,
        PhysicalASAPOperatorPayload::SummaryAgg { family: FieldDataType::Sketch(kind, _), .. } if kind.algorithm() == &algorithm)).unwrap();
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
                PhysicalASAPOperatorPayload::Relational {
                    operator: planner_types::ir::export::NonASAPOpKind::TimeRange { .. }
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
        &[u64::from(dag.roots[0].0)],
    )
    .unwrap();
    let bytes = serde_json::to_vec(&raw_compiled).unwrap();
    let raw_compiled =
        serde_json::from_slice::<asap_executor::physical_planner::CompiledPhysicalDAG>(&bytes)
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
            let source =
                Box::new(Operator::source(raw_schema.clone(), vec![raw_batch.clone()]).unwrap())
                    as Source<'static>;
            let physical_dag = raw_compiled
                .instantiate(BTreeMap::from([(u64::from(raw.id.0), source)]))
                .unwrap();
            let context = RunContext::new(scope, Limits::default()).unwrap();
            let mut raw_scores = block_on(async {
                let mut scores = Vec::new();
                let mut stream = physical_dag
                    .execute(&[u64::from(dag.roots[0].0)], context)
                    .unwrap()
                    .remove(0);
                while let Some(batch) = stream.next().await {
                    let batch = batch.unwrap();
                    for row in batch.rows() {
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
        &[u64::from(dag.roots[0].0)],
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
                series_row(
                    &schema,
                    &BTreeMap::from([
                        ("job".into(), "api".into()),
                        ("service".into(), service.into()),
                    ]),
                    time,
                    value,
                )
                .unwrap()
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
            let source = Box::new(Operator::source(schema.clone(), vec![batch.clone()]).unwrap())
                as Source<'static>;
            let physical_dag = compiled
                .instantiate(BTreeMap::from([(u64::from(input_id.0), source)]))
                .unwrap();
            let context = RunContext::new(scope, Limits::default()).unwrap();
            let mut scores = block_on(async {
                let mut scores = vec![];
                let mut stream = physical_dag
                    .execute(&[u64::from(dag.roots[0].0)], context)
                    .unwrap()
                    .remove(0);
                while let Some(batch) = stream.next().await {
                    let batch = batch.unwrap();
                    for row in batch.rows() {
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

/// Every summary state of `candidate` maintained at ingestion time, the
/// materialization a deployment would assign for a continuously served query:
/// each `SummaryAgg` and every input it consumes run at ingestion time, the
/// rest at query time. The phases are assigned on the exported DAG because
/// the candidate pins its finalize boundary to query time.
fn continuously_maintained_dag(candidate: &Rc<planner_types::ir::OperatorNode>) -> PhysicalASAPDAG {
    use planner_types::ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo};
    let timed = apply_materialization_timings(
        candidate,
        &MaterializationAssignment::all_query_time(),
        &mut TimingMemo::new(),
    )
    .unwrap();
    let dag = planner_types::ir::export::compile_physical_asap_dag(&timed).unwrap();
    let mut pending: Vec<_> = dag
        .nodes
        .iter()
        .filter(|node| matches!(node.payload, PhysicalASAPOperatorPayload::SummaryAgg { .. }))
        .map(|node| node.id)
        .collect();
    let mut ingestion = std::collections::HashSet::new();
    while let Some(id) = pending.pop() {
        if ingestion.insert(id) {
            pending.extend(
                dag.edges
                    .iter()
                    .filter(|edge| edge.consumer == id)
                    .map(|edge| edge.producer),
            );
        }
    }
    let phases = dag
        .nodes
        .iter()
        .map(|node| {
            let timing = if ingestion.contains(&node.id) {
                ExecutionTiming::IngestionTime
            } else {
                ExecutionTiming::QueryTime
            };
            (node.id, timing)
        })
        .collect();
    dag.with_execution_phases(&phases).unwrap()
}

// A maintained heap over finalized per-series Rate is the fixed-window
// placement: materialization timing, not a separate candidate, puts it in precompute.
#[test]
fn maintained_rate_heap_compiles_fixed_window_precompute() {
    use asap_executor::physical_planner::{compile_candidate, promql_rows::with_series_identity};
    let root = Rc::new(
        with_series_identity(
            &lower_promql("topk by(job)(2, rate(m[1m]))", AccuracyTarget::Epsilon(0.1)).unwrap(),
        )
        .unwrap(),
    );
    // The heap over the Rate realized as an exact accumulator. Stage 1 proves
    // no sign for rate values, so only the Count Sketch heap is executable.
    let candidates = stage1_candidates(&root)
        .into_iter()
        .filter(|candidate| {
            let dag = compile_physical_asap_dag(candidate).unwrap();
            builds(&dag, &SketchAlgorithm::CountSketchWithHeap)
                && dag.nodes.iter().any(|node| {
                    matches!(
                        &node.payload,
                        PhysicalASAPOperatorPayload::SummaryAgg {
                            family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
                            ..
                        }
                    )
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(candidates.len(), 1);
    for root in candidates {
        let dag = continuously_maintained_dag(&root);
        let state = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    &node.payload,
                    PhysicalASAPOperatorPayload::SummaryAgg {
                        family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
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
                    PhysicalASAPOperatorPayload::SummaryAgg {
                        family: FieldDataType::Sketch(..),
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
            &[u64::from(dag.roots[0].0)],
            &[u64::from(heap.id.0)],
        )
        .unwrap();
        let exported =
            asap_executor::physical_planner::promql_rows::compile_fixed_window_rate_aggregation(
                &dag,
            )
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&exported).unwrap(),
            serde_json::to_vec(&physical).unwrap()
        );
        // Execute the selected split across a state serialization boundary.
        // Each run builds fresh weights from that window's counters.
        let execute = |plan: &asap_executor::physical_planner::CompiledPhysicalDAG,
                       input: Batch,
                       scope: Scope| {
            let id = plan.input_contracts().next().unwrap().0;
            let source = Box::new(Operator::source(input.schema().clone(), vec![input]).unwrap())
                as Source<'static>;
            let physical_dag = plan.instantiate(BTreeMap::from([(id, source)])).unwrap();
            block_on(async {
                let mut stream = physical_dag
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
            PhysicalASAPOperatorPayload::SummaryAgg {
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
                        asap_executor::factory::create_planner_accumulator(family, input, grouping)
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
                            FieldDataType::ExactAggregate(..) => summary.clone(),
                            FieldDataType::Plain(DataType::Timestamp) => Value::Timestamp(end),
                            FieldDataType::Plain(DataType::Utf8)
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
                            FieldDataType::Plain(DataType::Utf8) => Value::Utf8("api".into()),
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
