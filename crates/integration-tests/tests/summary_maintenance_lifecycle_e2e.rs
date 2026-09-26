//! End-to-end coverage for workload-aware summary-maintenance planning:
//! source workload -> PromQL lowering -> candidate search ->
//! summary-maintenance lifecycle selection -> materialized deployment guarantees.

use std::rc::Rc;

use asap_aware_mapping::cost_model::Cost;
use asap_aware_mapping::CostRate;
use asap_aware_mapping::{
    assemble_selected_dag_with_summary_maintenance_lifecycles, export_summary_maintenance_plan,
    global_selection_with_summary_maintenance_lifecycles, search_workload_with, CostModel, Horizon,
    SummaryMaintenanceCapabilities, SummaryMaintenanceLifecycleCapabilities,
    SummaryMaintenanceLifecycleCostInputs, SummaryMaintenanceLifecycleRejection, WorkloadDemand,
};
use asap_frontend_promql::lower_promql_workload;
use asap_types::post_asap::{
    EvaluationSchedule, SummaryMaintenanceLifecycle, SummaryMaintenanceMode, SummaryNode,
};
use asap_types::pre_asap::agg_intent::AggIntent;
use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataWorkload, DurationMs, Evidence,
    EvidenceSource, PlanningWorkload, Predictability, Query, QueryLanguage, QueryRequirements,
    QueryTimeScope, QueryWorkload, Rate, RepeatedDemand, RepeatingEntry, RepetitionInterval,
    TimeSelection,
};

const NOW_MS: u64 = 1_000_000;

struct FullyCostedRuntime;

impl CostModel for FullyCostedRuntime {
    fn raw_query_recompute_total_cost(
        &self,
        _target: &asap_types::pre_asap::QueryExpr,
        _expected_reads: f64,
    ) -> Option<Cost> {
        Some(Cost(1_000.0))
    }

    fn rank_candidates(
        &self,
        _intent: &AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        candidates.to_vec()
    }

    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        SummaryMaintenanceLifecycleCostInputs {
            build_cost: Some(Cost(10.0)),
            maintenance_cost_per_update: Some(Cost(1.0)),
            summary_read_cost: Some(Cost(1.0)),
            retention_cost_rate: Some(CostRate(0.1)),
            retirement_cost: Some(Cost(1.0)),
        }
    }

    fn summary_maintenance_capabilities(
        &self,
        _summary: &SummaryNode,
    ) -> SummaryMaintenanceCapabilities {
        SummaryMaintenanceCapabilities {
            incremental_update: true,
            merge: true,
            delete: true,
        }
    }
}

fn dashboard_workload() -> PlanningWorkload {
    let query = Query("quantile_over_time(0.99, latency[5m])".into());
    let requirements = QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Epsilon(0.01)),
        ..QueryRequirements::default()
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: query.clone(),
                requirements: requirements.clone(),
                predictability: Predictability::AdHoc,
                invocations: 1,
                execute_at: None,
                time_selection: TimeSelection::default(),
            }]),
            repeating_queries: Some(vec![RepeatingEntry {
                query,
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(1_000)),
                requirements,
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection {
                    scope: QueryTimeScope::RealTime,
                    ..TimeSelection::default()
                },
            }]),
        },
        data_workload: Some(DataWorkload {
            arrival: DataArrival::ContinuouslyIngesting,
            ingestion_rate: Evidence {
                value: Some(Rate(1.0)),
                source: EvidenceSource::Observed,
                observed_at_ms: Some(NOW_MS),
                valid_for_ms: Some(60_000),
            },
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1_000)),
                ..Default::default()
            },
            ..DataWorkload::default()
        }),
    }
}

#[test]
fn promql_dashboard_materializes_continuous_summary_with_explained_rejections() {
    let workload = dashboard_workload();
    let plan = selected_plan(&workload);

    assert!(!plan.selected_raw_recompute);
    assert_eq!(plan.expected_reads, Some(100.0));
    assert_eq!(plan.deployments.len(), 1);

    let deployment = &plan.deployments[0];
    let guarantee = deployment
        .summary_maintenance_lifecycle_guarantee
        .as_ref()
        .expect("selected lifecycle guarantee");
    assert_eq!(
        guarantee.summary_maintenance_lifecycle,
        SummaryMaintenanceLifecycle::ContinuouslyMaintained
    );
    assert_eq!(guarantee.evaluation_schedule, EvaluationSchedule::PerUpdate);
    assert_eq!(
        guarantee.summary_maintenance_mode,
        SummaryMaintenanceMode::Incremental
    );
    assert!(deployment.alternatives.iter().any(|alternative| {
        matches!(
            alternative.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Prepared { .. }
        ) && alternative.rejection
            == Some(SummaryMaintenanceLifecycleRejection::RequiresPredictableOneTimeQuery)
    }));
    assert!(deployment.alternatives.iter().any(|alternative| {
        matches!(
            alternative.summary_maintenance_lifecycle,
            SummaryMaintenanceLifecycle::Shared { .. }
        ) && alternative.rejection
            == Some(SummaryMaintenanceLifecycleRejection::UnsupportedByRuntime)
    }));

    let exported = serde_json::to_value(export_summary_maintenance_plan(&plan)).unwrap();
    assert_eq!(
        exported["deployments"][0]["selected"]["lifecycle"]["kind"],
        "continuously_maintained"
    );
    assert_eq!(
        exported["deployments"][0]["selected"]["maintenance_mode"],
        "incremental"
    );
    let alternatives = exported["deployments"][0]["alternatives"]
        .as_array()
        .expect("exported lifecycle alternatives");
    assert!(alternatives.iter().any(|alternative| {
        alternative["lifecycle"]["kind"] == "prepared"
            && alternative["rejection"] == "requires_predictable_one_time_query"
    }));
    assert!(alternatives.iter().any(|alternative| {
        alternative["lifecycle"]["kind"] == "shared"
            && alternative["rejection"] == "unsupported_by_runtime"
    }));
    assert!(exported["graph"]["nodes"].as_array().is_some());
    let summary_node = exported["graph"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == "SummaryAgg")
        .expect("exported SummaryAgg node");
    assert_eq!(
        summary_node["detail"]["summary_maintenance"]["selected"]["lifecycle"]["kind"],
        "continuously_maintained"
    );
}

fn selected_plan(
    workload: &PlanningWorkload,
) -> asap_aware_mapping::SummaryMaintenanceLifecyclePlan {
    selected_plan_with_model(workload, &FullyCostedRuntime)
}

fn selected_plan_with_model(
    workload: &PlanningWorkload,
    model: &dyn CostModel,
) -> asap_aware_mapping::SummaryMaintenanceLifecyclePlan {
    selected_plan_with_horizon(workload, model, Horizon(100.))
}

fn selected_plan_with_horizon(
    workload: &PlanningWorkload,
    model: &dyn CostModel,
    horizon: Horizon,
) -> asap_aware_mapping::SummaryMaintenanceLifecyclePlan {
    workload.validate().unwrap();

    let lowered = lower_promql_workload(workload, 0)
        .expect("valid PromQL workload")
        .into_iter()
        .next()
        .expect("one normalized workload entry");
    let root = Rc::new(lowered);
    let strategies = asap_aware_mapping::default_strategies_with(model);
    let space = search_workload_with(vec![("dashboard", Rc::clone(&root))], &strategies);
    let target = Rc::clone(&space.roots[0].1);
    let capabilities = SummaryMaintenanceLifecycleCapabilities {
        supports_ephemeral: true,
        supports_prepared: false,
        supports_shared: false,
        supports_continuously_maintained: true,
    };

    let selection = global_selection_with_summary_maintenance_lifecycles(
        &space,
        WorkloadDemand {
            workload: &workload.query_workload,
            data_workload: workload.data_workload.as_ref(),
            entry_indices: &[1],
        },
        NOW_MS,
        Some(horizon),
        capabilities,
        model,
    )
    .unwrap();
    assemble_selected_dag_with_summary_maintenance_lifecycles(
        &selection,
        &target,
        WorkloadDemand::new_with_data(
            &workload.query_workload,
            workload.data_workload.as_ref().unwrap(),
            &[1],
        ),
        NOW_MS,
        Some(horizon),
        capabilities,
        model,
    )
    .unwrap()
    .expect("selected summary plan")
}

mod physical_common;

/// A selected continuous lifecycle supplies a materialization boundary; its
/// maintenance and query DAGs execute the selected KLL computation in fresh runs.
#[test]
fn continuous_lifecycle_compiles_and_executes_spatial_kll() {
    use asap_physical_operators::{
        physical_planner::{compile_candidate, InputContract},
        runtime::Scope,
        values::{Batch, Value},
    };
    use asap_types::{
        post_asap::{compile_executable_dag, ExecutableOperatorPayload, SummaryFamilyType},
        pre_asap::DataType,
    };
    use std::{collections::BTreeMap, sync::Arc};
    let mut workload = dashboard_workload();
    workload.query_workload.query_batch.as_mut().unwrap()[0].query =
        Query("quantile(0.99, latency)".into());
    workload.query_workload.repeating_queries.as_mut().unwrap()[0].query =
        Query("quantile(0.99, latency)".into());
    let selected = selected_plan(&workload);
    assert_eq!(
        selected.deployments[0]
            .summary_maintenance_lifecycle_guarantee
            .as_ref()
            .unwrap()
            .summary_maintenance_lifecycle,
        SummaryMaintenanceLifecycle::ContinuouslyMaintained
    );
    let dag = compile_executable_dag(&selected.root).unwrap();
    let build = dag
        .nodes
        .iter()
        .find(|node| matches!(node.payload, ExecutableOperatorPayload::SummaryAgg { .. }))
        .unwrap();
    let input = dag
        .edges
        .iter()
        .find(|edge| edge.consumer == build.id)
        .unwrap()
        .producer;
    let raw = dag.nodes.iter().find(|node| node.id == input).unwrap();
    let schema = Arc::new(raw.output_schema.clone());
    let candidate = compile_candidate(
        &dag,
        BTreeMap::from([(u64::from(input.0), InputContract::bounded(schema.clone()))]),
        &[u64::from(dag.root.0)],
        &[u64::from(build.id.0)],
    )
    .unwrap();

    // A continuous input without a finite pane boundary cannot implement this
    // blocking builder. Retain lifecycle ownership in the candidate payload;
    // only the legal bounded request candidate reaches workload pricing.
    let mut unbounded = InputContract::bounded(schema.clone());
    unbounded.properties.boundedness = asap_physical_operators::plan::Boundedness::Unbounded;
    let rejected = compile_candidate(
        &dag,
        BTreeMap::from([(u64::from(input.0), unbounded)]),
        &[u64::from(dag.root.0)],
        &[u64::from(build.id.0)],
    );
    assert!(rejected.is_err());
    let request = compile_candidate(
        &dag,
        BTreeMap::from([(u64::from(input.0), InputContract::bounded(schema.clone()))]),
        &[u64::from(dag.root.0)],
        &[],
    )
    .unwrap();
    let mut priced = 0;
    let feedback = asap_physical_operators::physical_planner::select_candidate(
        vec![
            rejected.map(|candidate| {
                (
                    SummaryMaintenanceLifecycle::ContinuouslyMaintained,
                    candidate,
                )
            }),
            Ok((SummaryMaintenanceLifecycle::Ephemeral, request)),
        ],
        |_| {
            priced += 1;
            Ok(Some(
                asap_physical_operators::physical_planner::CandidateCost {
                    workload_scope: "dashboard".into(),
                    horizon_seconds: 100.,
                    total_cost: 1000.,
                },
            ))
        },
    )
    .unwrap();
    assert_eq!(priced, 1);
    assert_eq!(feedback.candidate.0, SummaryMaintenanceLifecycle::Ephemeral);
    for revision in [1, 2] {
        let rows = (1..=100)
            .map(|value| {
                schema
                    .fields
                    .iter()
                    .map(|field| match field.dtype {
                        SummaryFamilyType::Plain(DataType::Float64) => {
                            Value::Float64(f64::from(value))
                        }
                        SummaryFamilyType::Plain(DataType::Timestamp) => Value::Timestamp(300_000),
                        _ => panic!("unexpected field {field:?}"),
                    })
                    .collect()
            })
            .collect();
        let raw_batch = Batch::try_new(schema.clone(), rows).unwrap();
        let direct = physical_common::execute(
            &feedback.candidate.1.query,
            BTreeMap::from([(u64::from(input.0), raw_batch.clone())]),
            Scope::Query {
                evaluation_time_ms: 300_000,
                revision,
            },
        );
        let state = physical_common::execute(
            candidate.precompute.as_ref().unwrap(),
            BTreeMap::from([(u64::from(input.0), raw_batch)]),
            Scope::Ingestion {
                window_start_ms: 0,
                window_end_ms: 300_000,
                revision,
            },
        );
        let result = physical_common::execute(
            &candidate.query,
            BTreeMap::from([(u64::from(build.id.0), state[0][0].clone())]),
            Scope::Query {
                evaluation_time_ms: 300_000,
                revision,
            },
        );
        let values: Vec<_> = result[0]
            .iter()
            .flat_map(|batch| batch.rows())
            .flat_map(|row| row.iter())
            .filter_map(|value| {
                if let Value::Float64(value) = value {
                    Some(*value)
                } else {
                    None
                }
            })
            .collect();
        let direct_values: Vec<_> = direct[0]
            .iter()
            .flat_map(|batch| batch.rows())
            .flat_map(|row| row.iter())
            .filter_map(|value| {
                if let Value::Float64(value) = value {
                    Some(*value)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            values, direct_values,
            "maintenance and request candidates preserve the same population"
        );
        assert_eq!(values.len(), 1);
        assert!(
            (98. ..=100.).contains(&values[0]),
            "p99 rank must reflect the supplied population"
        );
    }
}

struct SlidingPaneModel;
impl CostModel for SlidingPaneModel {
    fn raw_query_recompute_total_cost(
        &self,
        target: &asap_types::pre_asap::QueryExpr,
        reads: f64,
    ) -> Option<Cost> {
        let _ = (target, reads);
        Some(Cost(100_000.0))
    }
    fn rank_candidates(
        &self,
        intent: &AggIntent,
        candidates: &[asap_types::post_asap::SketchAlgorithm],
    ) -> Vec<asap_types::post_asap::SketchAlgorithm> {
        FullyCostedRuntime.rank_candidates(intent, candidates)
    }
    fn summary_maintenance_lifecycle_cost_inputs(
        &self,
        summary: &SummaryNode,
    ) -> SummaryMaintenanceLifecycleCostInputs {
        let mut costs = FullyCostedRuntime.summary_maintenance_lifecycle_cost_inputs(summary);
        // Controlled workload evidence makes repeated raw construction more
        // expensive than retaining and updating the same temporal population.
        costs.build_cost = Some(Cost(1000.));
        costs
    }
    fn summary_maintenance_capabilities(
        &self,
        summary: &SummaryNode,
    ) -> SummaryMaintenanceCapabilities {
        FullyCostedRuntime.summary_maintenance_capabilities(summary)
    }
    fn complete_summary_candidate_estimate(
        &self,
        _root: &SummaryNode,
        _target: Option<&asap_types::pre_asap::QueryExpr>,
        deployments: &[asap_aware_mapping::cost_model::CostedSummaryDeployment<'_>],
        _horizon: Option<Horizon>,
        _reads: Option<f64>,
        _accuracy: &[AccuracyTarget],
    ) -> Option<asap_aware_mapping::CompleteSummaryCandidateEstimate> {
        Some(asap_aware_mapping::CompleteSummaryCandidateEstimate {
            cost: Cost(
                deployments
                    .iter()
                    .map(|deployment| deployment.selected_cost.0)
                    .sum(),
            ),
            physical_plan_id: Some("bounded-sliding-pane-evidence".into()),
            window_frameworks: deployments
                .iter()
                .map(|deployment| {
                    (deployment.guarantee.summary_maintenance_lifecycle
                        == SummaryMaintenanceLifecycle::ContinuouslyMaintained)
                        .then_some(asap_types::post_asap::SummaryWindowFramework::Sliding)
                })
                .collect(),
            window_accuracy_guarantee: None,
        })
    }
}

/// Workload and optimizer-selected lifecycle generate both physical DAGs.
/// No computational operators or graph edges are constructed by this fixture.
#[test]
fn selected_temporal_lifecycle_compiles_panes_and_executes() {
    use asap_physical_operators::{
        operators::Operator,
        physical_planner::{
            compile_temporal_pane_candidate, InputContract, Source, TemporalEntityIdentity,
            TemporalPaneMaintenance,
        },
        runtime::{Limits, RunContext, Scope},
        summary_kernels::datasketches_kll::DatasketchesKLLAccumulator,
        values::{Batch, Value},
    };
    use asap_types::{
        post_asap::{
            compile_executable_dag, plan_pane_phase, ExecutableOperatorPayload, SummaryFamilyType,
            SummaryWindowFramework,
        },
        pre_asap::DataType,
        workload::TimestampMs,
    };
    use futures::{executor::block_on, StreamExt};
    use std::{collections::BTreeMap, sync::Arc};
    for quantile in [0.5, 0.99] {
        let mut workload = dashboard_workload();
        let query = Query(format!(
            "quantile_over_time({quantile}, latency{{job=\"api\"}}[5m])"
        ));
        workload.query_workload.query_batch.as_mut().unwrap()[0].query = query.clone();
        workload.query_workload.repeating_queries.as_mut().unwrap()[0].query = query;

        workload
            .data_workload
            .as_mut()
            .unwrap()
            .data_ingestion_interval
            .value = Some(DurationMs(60_000));
        workload.query_workload.repeating_queries.as_mut().unwrap()[0].demand =
            RepeatedDemand::FixedIntervalAt {
                interval: RepetitionInterval(60_000),
                evaluation_phase: TimestampMs(300_000),
            };
        let plan = selected_plan_with_horizon(&workload, &SlidingPaneModel, Horizon(1000.));
        assert!(!plan.selected_raw_recompute);
        assert_eq!(plan.deployments.len(), 1);
        let deployment = &plan.deployments[0];
        assert_eq!(
            deployment.selected_window_framework,
            Some(SummaryWindowFramework::Sliding)
        );
        let dag = compile_executable_dag(&plan.root).unwrap();
        let build = dag
            .nodes
            .iter()
            .find(|node| matches!(node.payload, ExecutableOperatorPayload::SummaryAgg { .. }))
            .unwrap();
        assert_eq!(build.id, deployment.post_asap_node_id);
        let raw = dag
            .nodes
            .iter()
            .find(|node| matches!(node.payload, ExecutableOperatorPayload::Fallback { .. }))
            .unwrap();
        let schema = Arc::new(raw.output_schema.clone());
        let width = workload
            .data_workload
            .as_ref()
            .unwrap()
            .data_ingestion_interval
            .value
            .unwrap()
            .0;
        let layout = plan_pane_phase(
            &workload.query_workload.repeating_queries.as_ref().unwrap()[0].demand,
            width,
        )
        .unwrap();
        let maintenance = TemporalPaneMaintenance {
            summary_node: u64::from(build.id.0),
            lifecycle: deployment
                .summary_maintenance_lifecycle_guarantee
                .clone()
                .unwrap(),
            framework: deployment.selected_window_framework.clone().unwrap(),
            layout,
            // The memory source has exactly the declared label columns; a
            // schemaless deployment must resolve all entity keys first.
            entity_identity: TemporalEntityIdentity::Columns(
                schema
                    .fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| field.name == "job")
                    .map(|(index, _)| index)
                    .collect(),
            ),
        };
        let candidate = compile_temporal_pane_candidate(
            &dag,
            BTreeMap::from([(u64::from(raw.id.0), InputContract::bounded(schema.clone()))]),
            &[u64::from(dag.root.0)],
            &maintenance,
        )
        .unwrap();
        assert_eq!(candidate.window_width_ms, 300_000);
        assert_eq!(candidate.pane_inputs.len(), 5);
        assert_ne!(
            candidate.physical.precompute.as_ref().unwrap().roots()[0],
            maintenance.summary_node,
            "a one-minute pane is not the logical five-minute summary output"
        );
        let source_opens = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut stored = Vec::new();
        for pane in 0..6 {
            let rows = (0..20)
                .flat_map(|sample| {
                    ["api", "batch"]
                        .into_iter()
                        .map(move |entity| (sample, entity))
                })
                .map(|(sample, entity)| {
                    schema
                        .fields
                        .iter()
                        .map(|field| match field.dtype {
                            SummaryFamilyType::Plain(DataType::Timestamp) => {
                                Value::Timestamp(pane * 60_000 + (sample + 1) * 3000)
                            }
                            SummaryFamilyType::Plain(DataType::Float64) => Value::Float64(
                                (pane * 20 + sample) as f64
                                    + if entity == "batch" { 100_000. } else { 0. },
                            ),
                            SummaryFamilyType::Plain(DataType::Utf8) => Value::Utf8(entity.into()),
                            _ => panic!("unexpected raw field {field:?}"),
                        })
                        .collect()
                })
                .collect();
            let result = physical_common::execute(
                candidate.physical.precompute.as_ref().unwrap(),
                BTreeMap::from([(
                    u64::from(raw.id.0),
                    Batch::try_new(schema.clone(), rows).unwrap(),
                )]),
                Scope::Ingestion {
                    window_start_ms: pane * 60_000,
                    window_end_ms: (pane + 1) * 60_000,
                    revision: 1,
                },
            );
            let batch = &result[0][0];
            let rows = batch
                .rows()
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| match value {
                            Value::Summary { family, state } => Value::Summary {
                                family: family.clone(),
                                state: Arc::new(
                                    DatasketchesKLLAccumulator::from_msgpack_bytes(
                                        &state.serialize_to_bytes(),
                                    )
                                    .unwrap(),
                                ),
                            },
                            value => value.clone(),
                        })
                        .collect()
                })
                .collect();
            stored.push(Batch::try_new(batch.schema().clone(), rows).unwrap());
        }
        for offset in [0, 1] {
            let inputs: BTreeMap<_, _> = candidate
                .pane_inputs
                .iter()
                .enumerate()
                .map(|(index, &id)| (id, stored[offset + index].clone()))
                .collect();
            let scope = Scope::Query {
                evaluation_time_ms: (5 + offset as i64) * 60_000,
                revision: 1,
            };
            let result =
                physical_common::execute(&candidate.physical.query, inputs.clone(), scope.clone());
            let rows = result[0][0].rows();
            assert_eq!(rows.len(), 1);
            assert!(rows[0]
                .iter()
                .any(|value| matches!(value, Value::Utf8(label) if label.as_ref() == "api")));
            let Value::Float64(value) = rows[0][1] else {
                panic!("missing p99")
            };
            assert!(
                (value - ((if quantile == 0.5 { 50 } else { 99 }) + offset * 20) as f64).abs()
                    <= 1.
            );
            assert!(
                matches!(rows[0][0], Value::Timestamp(timestamp) if timestamp == (5 + offset as i64) * 60_000)
            );
            let sources = |inputs: BTreeMap<u64, Batch>| -> BTreeMap<u64, Source<'static>> {
                inputs
                    .into_iter()
                    .map(|(id, batch)| {
                        (
                            id,
                            Box::new(TemporalCountingSource {
                                operator: Operator::source(batch.schema().clone(), vec![batch])
                                    .unwrap(),
                                opens: source_opens.clone(),
                            }) as Source<'static>,
                        )
                    })
                    .collect()
            };
            let bound = candidate
                .physical
                .query
                .instantiate(sources(inputs.clone()))
                .unwrap();
            let population = block_on(
                bound
                    .execute(
                        &[candidate.merged_state],
                        RunContext::new(scope.clone(), Limits::default()).unwrap(),
                    )
                    .unwrap()
                    .remove(0)
                    .collect::<Vec<_>>(),
            );
            let Value::Summary { state, .. } = population[0].as_ref().unwrap().rows()[0]
                .iter()
                .find(|value| matches!(value, Value::Summary { .. }))
                .unwrap()
            else {
                panic!("missing merged state")
            };
            assert_eq!(
                state
                    .as_any()
                    .downcast_ref::<DatasketchesKLLAccumulator>()
                    .unwrap()
                    .inner
                    .count(),
                100
            );
            let mut missing = inputs.clone();
            missing.remove(&candidate.pane_inputs[0]);
            assert!(candidate
                .physical
                .query
                .instantiate(sources(missing))
                .is_err());
            let mut duplicate = inputs.clone();
            duplicate.insert(candidate.pane_inputs[1], stored[offset].clone());
            let bad = candidate
                .physical
                .query
                .instantiate(sources(duplicate))
                .unwrap();
            let errors = block_on(
                bad.execute(
                    candidate.physical.query.roots(),
                    RunContext::new(scope.clone(), Limits::default()).unwrap(),
                )
                .unwrap()
                .remove(0)
                .collect::<Vec<_>>(),
            );
            assert!(
                errors.iter().any(Result::is_err),
                "duplicate pane must not be merged twice"
            );
            let mut duplicate_entity = inputs.clone();
            let pane = &stored[offset];
            duplicate_entity.insert(
                candidate.pane_inputs[0],
                Batch::try_new(
                    pane.schema().clone(),
                    vec![pane.rows()[0].clone(), pane.rows()[0].clone()],
                )
                .unwrap(),
            );
            let bad = candidate
                .physical
                .query
                .instantiate(sources(duplicate_entity))
                .unwrap();
            let errors = block_on(
                bad.execute(
                    candidate.physical.query.roots(),
                    RunContext::new(scope.clone(), Limits::default()).unwrap(),
                )
                .unwrap()
                .remove(0)
                .collect::<Vec<_>>(),
            );
            assert!(
                errors.iter().any(Result::is_err),
                "duplicate snapshots within a pane must fail"
            );
            let bound = candidate
                .physical
                .query
                .instantiate(sources(inputs))
                .unwrap();
            source_opens.store(0, std::sync::atomic::Ordering::SeqCst);
            assert!(bound
                .execute(
                    candidate.physical.query.roots(),
                    RunContext::new(
                        Scope::Query {
                            evaluation_time_ms: 330_000,
                            revision: 1
                        },
                        Limits::default()
                    )
                    .unwrap()
                )
                .is_err());
            assert_eq!(
                source_opens.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "invalid phase must fail before opening readers"
            );
        }
        // A selected framework cannot be silently replaced by physical planning.
        let mut wrong_framework = maintenance.clone();
        wrong_framework.framework = SummaryWindowFramework::ExponentialHistogram;
        let mut wrong_identity = maintenance.clone();
        wrong_identity.entity_identity = TemporalEntityIdentity::SingleEntity;
        let mut unknown_phase = maintenance.clone();
        unknown_phase.layout.pane_origin_ms = None;
        let mut partial_panes = maintenance.clone();
        partial_panes.layout.pane_width_ms = 90_000;
        let mut wrong_lifecycle = maintenance.clone();
        wrong_lifecycle.lifecycle.summary_maintenance_lifecycle =
            SummaryMaintenanceLifecycle::Ephemeral;
        for unsupported in [
            wrong_framework,
            wrong_identity,
            unknown_phase,
            partial_panes,
            wrong_lifecycle,
        ] {
            assert!(compile_temporal_pane_candidate(
                &dag,
                BTreeMap::from([(u64::from(raw.id.0), InputContract::bounded(schema.clone()))]),
                &[u64::from(dag.root.0)],
                &unsupported
            )
            .is_err());
        }
    }
}

struct TemporalCountingSource {
    operator: asap_physical_operators::operators::Operator,
    opens: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl
    asap_physical_operators::plan::PhysicalOperator<
        asap_physical_operators::values::Batch,
        asap_physical_operators::values::Schema,
    > for TemporalCountingSource
{
    fn name(&self) -> &str {
        "TemporalCountingSource"
    }
    fn properties(
        &self,
        inputs: &[asap_physical_operators::plan::PlanProperties],
    ) -> asap_physical_operators::plan::PlanProperties {
        self.operator.properties(inputs)
    }
    fn input_schemas(&self) -> Vec<asap_physical_operators::values::Schema> {
        self.operator.input_schemas()
    }
    fn output_schema(&self) -> asap_physical_operators::values::Schema {
        self.operator.output_schema()
    }
    fn output_bytes(&self, batch: &asap_physical_operators::values::Batch) -> usize {
        batch.bytes()
    }
    fn start<'a>(
        &'a self,
        inputs: Vec<
            asap_physical_operators::runtime::Input<'a, asap_physical_operators::values::Batch>,
        >,
        context: asap_physical_operators::runtime::RunContext,
    ) -> Result<
        asap_physical_operators::runtime::OutputStream<'a, asap_physical_operators::values::Batch>,
        asap_physical_operators::Error,
    > {
        self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.operator.start(inputs, context)
    }
}
