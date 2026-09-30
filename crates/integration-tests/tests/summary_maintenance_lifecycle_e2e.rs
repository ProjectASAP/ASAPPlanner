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
    EvaluationSchedule, PostASAPNode, SummaryMaintenanceLifecycle, SummaryMaintenanceMode,
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
        _target: &asap_types::pre_asap::PreASAPNode,
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
        _summary: &PostASAPNode,
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
        _summary: &PostASAPNode,
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
    selected_plan_for_lowered(workload, lowered, model, horizon)
}

fn selected_plan_for_lowered(
    workload: &PlanningWorkload,
    lowered: asap_types::pre_asap::PreASAPNode,
    model: &dyn CostModel,
    horizon: Horizon,
) -> asap_aware_mapping::SummaryMaintenanceLifecyclePlan {
    let root = Rc::new(lowered);
    let strategies = asap_aware_mapping::default_strategies_with(model);
    let space = search_workload_with(vec![("dashboard", Rc::clone(&root))], &strategies);
    let target = Rc::clone(&space.roots()[0].1);
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
        post_asap::{export_post_asap_dag, PostASAPOperatorPayload, SummaryFamilyType},
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
    let dag = export_post_asap_dag(&selected.root).unwrap();
    let build = dag
        .nodes
        .iter()
        .find(|node| matches!(node.payload, PostASAPOperatorPayload::SummaryAgg { .. }))
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

fn quantile_workload(query: &str) -> PlanningWorkload {
    let mut workload = dashboard_workload();
    workload.query_workload.query_batch.as_mut().unwrap()[0].query = Query(query.into());
    workload.query_workload.repeating_queries.as_mut().unwrap()[0].query = Query(query.into());
    workload
}

/// Timed DAG for `query` after binding every summary state to `lifecycle`.
/// Grouped queries carry a physical series identity, as per-entity state needs.
fn lifecycle_timed_dag(
    query: &str,
    lifecycle: &SummaryMaintenanceLifecycle,
) -> (asap_types::post_asap::PostASAPDAGTransport, Vec<u64>) {
    let workload = quantile_workload(query);
    let mut lowered = lower_promql_workload(&workload, 0).unwrap().remove(0);
    if query.contains(" by(") {
        lowered =
            asap_physical_operators::physical_planner::promql_rows::with_series_identity(&lowered)
                .unwrap();
    }
    let root =
        selected_plan_for_lowered(&workload, lowered, &FullyCostedRuntime, Horizon(100.)).root;
    let candidates = asap_aware_mapping::CandidatePostASAPDAGsWithTiming::from_post_asap_dag(
        (),
        root,
        asap_aware_mapping::CandidateTimingContext {
            demand: WorkloadDemand::new_with_data(
                &workload.query_workload,
                workload.data_workload.as_ref().unwrap(),
                &[1],
            ),
            now_ms: NOW_MS,
            horizon: Some(Horizon(100.)),
            capabilities: SummaryMaintenanceLifecycleCapabilities::ALL,
            cost_model: &FullyCostedRuntime,
        },
        4096,
    )
    .unwrap();
    let choices: Vec<_> = candidates
        .lifecycle_alternatives(0)
        .unwrap()
        .iter()
        .map(|deployment| (deployment.post_asap_node_id, lifecycle.clone()))
        .collect();
    let mut states: Vec<_> = choices.iter().map(|(id, _)| u64::from(id.0)).collect();
    states.sort_unstable();
    let dag = candidates
        .select_lifecycles(0, &choices)
        .unwrap()
        .export_timed_dag()
        .unwrap();
    (dag, states)
}

/// Compile inputs for a timed DAG: its raw source, available at either phase.
fn raw_inputs(
    dag: &asap_types::post_asap::PostASAPDAGTransport,
) -> std::collections::BTreeMap<u64, asap_physical_operators::physical_planner::InputContract> {
    let raw = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                asap_types::post_asap::PostASAPOperatorPayload::Fallback { .. }
            )
        })
        .unwrap();
    std::collections::BTreeMap::from([(
        u64::from(raw.id.0),
        asap_physical_operators::physical_planner::InputContract::bounded(std::sync::Arc::new(
            raw.output_schema.clone(),
        )),
    )])
}

/// For existing PromQL fixtures, Planner's own retained lifecycle selection
/// reproduces the timing that realization strategies assign today.
#[test]
fn planner_lifecycle_selection_reproduces_strategy_timing() {
    for query in [
        "quantile_over_time(0.99, latency[5m])",
        "quantile(0.99, latency)",
        "sum by(job)(rate(m[1m]))",
    ] {
        let plan = selected_plan(&quantile_workload(query));
        assert!(!plan.selected_raw_recompute, "{query}");
        assert!(plan.deployments.iter().all(|deployment| {
            deployment
                .summary_maintenance_lifecycle_guarantee
                .as_ref()
                .is_some_and(|guarantee| {
                    guarantee.summary_maintenance_lifecycle
                        != SummaryMaintenanceLifecycle::Ephemeral
                })
        }));
        let strategy = asap_types::post_asap::export_post_asap_dag(&plan.root).unwrap();
        assert_eq!(plan.export_timed_dag().unwrap(), strategy, "{query}");
    }
}

/// An explicitly chosen lifecycle reaches physical compilation through timing:
/// ContinuouslyMaintained puts the state in precompute, Ephemeral leaves
/// precompute empty and reads the raw source at query time; both answer alike.
#[test]
fn chosen_lifecycle_timing_decides_precompute_contents() {
    use asap_physical_operators::{
        physical_planner::{compile_candidate, frontier_from_timing},
        runtime::Scope,
        values::{Batch, Value},
    };
    use asap_types::{post_asap::SummaryFamilyType, pre_asap::DataType};
    use std::collections::BTreeMap;

    let mut answers = Vec::new();
    for lifecycle in [
        SummaryMaintenanceLifecycle::ContinuouslyMaintained,
        SummaryMaintenanceLifecycle::Ephemeral,
    ] {
        let (dag, states) = lifecycle_timed_dag("quantile(0.99, latency)", &lifecycle);
        let [state] = states[..] else {
            panic!("one summary state");
        };
        let inputs = raw_inputs(&dag);
        let (&raw_id, contract) = inputs.iter().next().unwrap();
        let schema = contract.schema.clone();
        let frontier = frontier_from_timing(dag.as_view()).unwrap();
        let candidate =
            compile_candidate(&dag, inputs, &[u64::from(dag.root.0)], &frontier).unwrap();
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
        let query_scope = Scope::Query {
            evaluation_time_ms: 300_000,
            revision: 1,
        };
        let result = if lifecycle == SummaryMaintenanceLifecycle::Ephemeral {
            assert!(frontier.is_empty());
            assert!(candidate.precompute.is_none());
            physical_common::execute(
                &candidate.query,
                BTreeMap::from([(raw_id, raw_batch)]),
                query_scope,
            )
        } else {
            assert_eq!(frontier, [state]);
            assert_eq!(
                candidate
                    .materialized_outputs
                    .keys()
                    .copied()
                    .collect::<Vec<_>>(),
                [state]
            );
            let stored = physical_common::execute(
                candidate.precompute.as_ref().unwrap(),
                BTreeMap::from([(raw_id, raw_batch)]),
                Scope::Ingestion {
                    window_start_ms: 0,
                    window_end_ms: 300_000,
                    revision: 1,
                },
            );
            physical_common::execute(
                &candidate.query,
                BTreeMap::from([(state, stored[0][0].clone())]),
                query_scope,
            )
        };
        answers.push(
            result[0]
                .iter()
                .flat_map(|batch| batch.rows())
                .flat_map(|row| row.iter())
                .filter_map(|value| match value {
                    Value::Float64(value) => Some(*value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0].len(), 1);
}

/// One compilation, cut by each lifecycle assignment's timing, yields exactly
/// the candidate `compile_candidate` builds for that timed DAG: the retained
/// state is the frontier under ContinuouslyMaintained, and nothing under
/// Ephemeral. Covers the KLL quantile fixture and grouped Rate→Sum.
#[test]
fn lifecycle_timing_cuts_one_compilation() {
    use asap_physical_operators::physical_planner::{
        compile, compile_candidate, cut_candidate, frontier_from_timing,
    };
    for query in ["quantile(0.99, latency)", "sum by(job)(rate(m[1m]))"] {
        let ephemeral = SummaryMaintenanceLifecycle::Ephemeral;
        let (compiled_dag, _) = lifecycle_timed_dag(query, &ephemeral);
        let inputs = raw_inputs(&compiled_dag);
        let roots = [u64::from(compiled_dag.root.0)];
        let compiled = compile(compiled_dag.as_view(), inputs.clone(), &roots).unwrap();
        for lifecycle in [
            SummaryMaintenanceLifecycle::ContinuouslyMaintained,
            ephemeral,
        ] {
            let (dag, states) = lifecycle_timed_dag(query, &lifecycle);
            let frontier = frontier_from_timing(dag.as_view()).unwrap();
            // Retained states read by a query-time consumer, or the root itself.
            let query_time = |id: u64| {
                dag.nodes.iter().any(|node| {
                    u64::from(node.id.0) == id
                        && node.output_state.timing
                            == asap_types::post_asap::ExecutionTiming::QueryTime
                })
            };
            let expected_frontier = if lifecycle == SummaryMaintenanceLifecycle::Ephemeral {
                vec![]
            } else {
                states
                    .iter()
                    .copied()
                    .filter(|state| {
                        *state == u64::from(dag.root.0)
                            || dag.edges.iter().any(|edge| {
                                u64::from(edge.producer.0) == *state
                                    && query_time(u64::from(edge.consumer.0))
                            })
                    })
                    .collect()
            };
            assert_eq!(frontier, expected_frontier, "{query} {lifecycle:?}");
            let cut = cut_candidate(&compiled, &frontier).unwrap();
            let expected = compile_candidate(&dag, inputs.clone(), &roots, &frontier).unwrap();
            assert_eq!(
                serde_json::to_vec(&cut).unwrap(),
                serde_json::to_vec(&expected).unwrap(),
                "{query} {lifecycle:?}"
            );
        }
    }
}

/// A maintained current-series population is placed by its lifecycle choice:
/// ContinuouslyMaintained stores the population in precompute, Ephemeral
/// rebuilds it from the raw source at query time; both rank alike.
#[test]
fn chosen_population_lifecycle_decides_precompute_contents() {
    use asap_aware_mapping::maintained_population::MaintainedPopulationStrategy;
    use asap_physical_operators::{
        physical_planner::{
            compile_candidate,
            promql_rows::{series_row, with_series_identity},
            InputContract,
        },
        runtime::Scope,
        values::{Batch, Value},
    };
    use asap_types::post_asap::{
        maintained_population::PopulationInput, PostASAPOperatorPayload, ValueOperation,
    };
    use std::{collections::BTreeMap, sync::Arc};

    let workload = quantile_workload("topk by(job)(1, m)");
    let root = Rc::new(
        with_series_identity(&lower_promql_workload(&workload, 0).unwrap().remove(0)).unwrap(),
    );
    let root = MaintainedPopulationStrategy::new(std::slice::from_ref(&root))
        .candidate(&root)
        .unwrap();
    let mut answers = Vec::new();
    for lifecycle in [
        SummaryMaintenanceLifecycle::ContinuouslyMaintained,
        SummaryMaintenanceLifecycle::Ephemeral,
    ] {
        let candidates = asap_aware_mapping::CandidatePostASAPDAGsWithTiming::from_post_asap_dag(
            (),
            Rc::clone(&root),
            asap_aware_mapping::CandidateTimingContext {
                demand: WorkloadDemand::new_with_data(
                    &workload.query_workload,
                    workload.data_workload.as_ref().unwrap(),
                    &[1],
                ),
                now_ms: NOW_MS,
                horizon: Some(Horizon(100.)),
                capabilities: SummaryMaintenanceLifecycleCapabilities::ALL,
                cost_model: &FullyCostedRuntime,
            },
            4096,
        )
        .unwrap();
        let [deployment] = candidates.lifecycle_alternatives(0).unwrap() else {
            panic!("one population state");
        };
        let id = deployment.post_asap_node_id;
        let dag = candidates
            .select_lifecycles(0, &[(id, lifecycle.clone())])
            .unwrap()
            .export_timed_dag()
            .unwrap();
        let population = dag.nodes.iter().find(|node| node.id == id).unwrap();
        let PostASAPOperatorPayload::Value {
            operation: ValueOperation::MaintainPopulation { population },
        } = &population.payload
        else {
            panic!("the deployment is the maintained population");
        };
        let PopulationInput::CurrentSeries(spec) = &population.input else {
            panic!("current-series population");
        };
        let lookback = i64::try_from(spec.lookback_ms).unwrap();
        let raw = dag
            .nodes
            .iter()
            .find(|node| matches!(node.payload, PostASAPOperatorPayload::Fallback { .. }))
            .unwrap();
        let (raw_id, schema) = (u64::from(raw.id.0), Arc::new(raw.output_schema.clone()));
        let frontier =
            asap_physical_operators::physical_planner::frontier_from_timing(dag.as_view()).unwrap();
        let candidate = compile_candidate(
            &dag,
            BTreeMap::from([(raw_id, InputContract::bounded(schema.clone()))]),
            &[u64::from(dag.root.0)],
            &frontier,
        )
        .unwrap();
        let end = 60_000;
        let rows = [("a", end - 1, 100.), ("a", end, 1.), ("b", end, 20.)]
            .into_iter()
            .map(|(instance, at, value)| {
                series_row(
                    &schema,
                    &BTreeMap::from([
                        ("job".into(), "api".into()),
                        ("instance".into(), instance.into()),
                    ]),
                    at,
                    value,
                )
                .unwrap()
            })
            .collect();
        let raw_batch = Batch::try_new(schema.clone(), rows).unwrap();
        let query_scope = Scope::Query {
            evaluation_time_ms: end,
            revision: 1,
        };
        let result = if lifecycle == SummaryMaintenanceLifecycle::Ephemeral {
            assert!(frontier.is_empty());
            assert!(candidate.precompute.is_none());
            physical_common::execute(
                &candidate.query,
                BTreeMap::from([(raw_id, raw_batch)]),
                query_scope,
            )
        } else {
            let state = u64::from(id.0);
            assert_eq!(frontier, [state]);
            let stored = physical_common::execute(
                candidate.precompute.as_ref().unwrap(),
                BTreeMap::from([(raw_id, raw_batch)]),
                Scope::Ingestion {
                    window_start_ms: end - lookback,
                    window_end_ms: end,
                    revision: 1,
                },
            );
            physical_common::execute(
                &candidate.query,
                BTreeMap::from([(state, stored[0][0].clone())]),
                query_scope,
            )
        };
        answers.push(
            result[0]
                .iter()
                .flat_map(|batch| batch.rows())
                .flat_map(|row| row.iter())
                .filter_map(|value| match value {
                    Value::Float64(value) => Some(*value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(answers[0], [20.]);
}

/// Grouped Rate→Sum is one inventory candidate: retaining the Sum state puts
/// Rate and Sum in precompute, while an `Ephemeral` Sum over a retained Rate
/// state leaves Sum in the query DAG.
#[test]
fn grouped_rate_sum_placement_is_a_lifecycle_choice() {
    use asap_physical_operators::physical_planner::{compile_candidate, InputContract};
    use asap_types::post_asap::{
        ExactKind, PostASAPOperatorPayload, SummaryExpr, SummaryFamilyType,
    };
    use std::{collections::BTreeMap, sync::Arc};

    let workload = quantile_workload("sum by(job)(rate(m[1m]))");
    let root = Rc::new(
        asap_physical_operators::physical_planner::promql_rows::with_series_identity(
            &lower_promql_workload(&workload, 0).unwrap().remove(0),
        )
        .unwrap(),
    );
    let is_exact = |node: &PostASAPNode, kind: ExactKind| {
        matches!(&node.expr, SummaryExpr::SummaryAgg {
            family: SummaryFamilyType::ExactAggregate(k, _), ..
        } if *k == kind)
    };
    let inventory = asap_aware_mapping::search_workload(vec![("q", root)])
        .enumerate_candidate_dags(4096)
        .unwrap();
    let candidates = inventory
        .candidates
        .into_iter()
        .map(|mut forest| forest.remove(0).1)
        .filter(|candidate| {
            matches!(&candidate.expr, SummaryExpr::ValueOperation { child, .. }
                if is_exact(child, ExactKind::Sum))
        })
        .collect::<Vec<_>>();
    let [candidate] = candidates.as_slice() else {
        panic!("one grouped Sum candidate, got {}", candidates.len());
    };
    let mut placements = Vec::new();
    for sum_lifecycle in [
        SummaryMaintenanceLifecycle::ContinuouslyMaintained,
        SummaryMaintenanceLifecycle::Ephemeral,
    ] {
        let lifecycles = asap_aware_mapping::CandidatePostASAPDAGsWithTiming::from_post_asap_dag(
            (),
            Rc::clone(candidate),
            asap_aware_mapping::CandidateTimingContext {
                demand: WorkloadDemand::new_with_data(
                    &workload.query_workload,
                    workload.data_workload.as_ref().unwrap(),
                    &[1],
                ),
                now_ms: NOW_MS,
                horizon: Some(Horizon(100.)),
                capabilities: SummaryMaintenanceLifecycleCapabilities::ALL,
                cost_model: &FullyCostedRuntime,
            },
            4096,
        )
        .unwrap();
        let choices = lifecycles
            .lifecycle_alternatives(0)
            .unwrap()
            .iter()
            .map(|deployment| {
                let lifecycle = if is_exact(&deployment.summary, ExactKind::Sum) {
                    sum_lifecycle.clone()
                } else {
                    SummaryMaintenanceLifecycle::ContinuouslyMaintained
                };
                (deployment.post_asap_node_id, lifecycle)
            })
            .collect::<Vec<_>>();
        assert_eq!(choices.len(), 2, "Rate and Sum states");
        let dag = lifecycles
            .select_lifecycles(0, &choices)
            .unwrap()
            .export_timed_dag()
            .unwrap();
        let raw = dag
            .nodes
            .iter()
            .find(|node| matches!(node.payload, PostASAPOperatorPayload::Fallback { .. }))
            .unwrap();
        let frontier =
            asap_physical_operators::physical_planner::frontier_from_timing(dag.as_view()).unwrap();
        let [boundary] = frontier.as_slice() else {
            panic!("one precompute output, got {frontier:?}");
        };
        let boundary = dag
            .nodes
            .iter()
            .find(|node| u64::from(node.id.0) == *boundary)
            .unwrap();
        let physical = compile_candidate(
            &dag,
            BTreeMap::from([(
                u64::from(raw.id.0),
                InputContract::bounded(Arc::new(raw.output_schema.clone())),
            )]),
            &[u64::from(dag.root.0)],
            &frontier,
        )
        .unwrap();
        let json = |value| String::from_utf8(serde_json::to_vec(value).unwrap()).unwrap();
        placements.push((
            boundary.payload.clone(),
            json(physical.precompute.as_ref().unwrap()),
            json(&physical.query),
        ));
    }
    let builds = |json: &str, kind: &str| {
        json.contains(&format!(
            r#"{{"SummaryBuild":{{"family":{{"ExactAggregate":["{kind}","{kind}"]}}"#
        ))
    };
    let [(retained, retained_pre, retained_query), (ephemeral, ephemeral_pre, ephemeral_query)] =
        placements.as_slice()
    else {
        unreachable!()
    };
    let state = |payload: &PostASAPOperatorPayload, kind: ExactKind| {
        matches!(payload, PostASAPOperatorPayload::SummaryAgg {
            family: SummaryFamilyType::ExactAggregate(k, _), ..
        } if *k == kind)
    };
    assert!(state(retained, ExactKind::Sum));
    assert!(builds(retained_pre, "Rate") && builds(retained_pre, "Sum"));
    assert!(!retained_query.contains("SummaryBuild"));
    assert!(state(ephemeral, ExactKind::Rate));
    assert!(builds(ephemeral_pre, "Rate") && !builds(ephemeral_pre, "Sum"));
    assert!(builds(ephemeral_query, "Sum"));
}

/// The lifecycle-timed DAG Planner selects for `query` with upfront series
/// typing, and whether it keeps an ingestion-time Binary.
fn typed_selection(query: &str) -> (asap_types::post_asap::PostASAPDAGTransport, bool) {
    use asap_types::post_asap::{ExecutionTiming, PostASAPOperatorPayload};
    let workload = quantile_workload(query);
    let lowered = asap_types::pre_asap::schema::with_promql_series_identity(
        &lower_promql_workload(&workload, 0).unwrap().remove(0),
    )
    .unwrap();
    let dag = selected_plan_for_lowered(&workload, lowered, &FullyCostedRuntime, Horizon(100.))
        .export_timed_dag()
        .unwrap();
    let ingestion_binary = dag.nodes.iter().any(|node| {
        matches!(node.payload, PostASAPOperatorPayload::Binary { .. })
            && node.output_state.timing == ExecutionTiming::IngestionTime
    });
    (dag, ingestion_binary)
}

/// Execute a timed DAG's precompute and query graphs over `samples`
/// (`(metric, job, seconds, value)`) at 300s; returns the root's values.
fn execute_timed(
    dag: &asap_types::post_asap::PostASAPDAGTransport,
    samples: &[(&str, &str, i64, f64)],
) -> Vec<f64> {
    use asap_physical_operators::{
        physical_planner::{
            compile_candidate, frontier_from_timing, promql_fallback, promql_rows, InputContract,
        },
        runtime::Scope,
        values::{Batch, Value},
    };
    use asap_types::{
        post_asap::PostASAPOperatorPayload,
        pre_asap::{PreASAPNode, Source},
    };
    use std::{collections::BTreeMap, sync::Arc};
    // Raw inputs: a selector Fallback is itself the input; a retained
    // expression reads each of its selectors through its raw-series slots.
    let mut raw = BTreeMap::new();
    for node in &dag.nodes {
        let PostASAPOperatorPayload::Fallback { expression } = &node.payload else {
            continue;
        };
        let metric = |selector: &PreASAPNode| match selector {
            PreASAPNode::TimeRange { child, .. } => match child.as_ref() {
                PreASAPNode::Scan {
                    source: Source::TimeSeries { metric },
                    ..
                } => Some(metric.clone()),
                _ => None,
            },
            PreASAPNode::Scan {
                source: Source::TimeSeries { metric },
                ..
            } => Some(metric.clone()),
            _ => None,
        };
        if let Some(name) = metric(expression) {
            raw.insert(
                u64::from(node.id.0),
                (Arc::new(node.output_schema.clone()), name),
            );
        } else {
            for (i, (selector, schema)) in promql_fallback::raw_series(expression)
                .unwrap()
                .into_iter()
                .enumerate()
            {
                raw.insert(
                    promql_fallback::raw_series_input(u64::from(node.id.0), i),
                    (schema, metric(&selector).unwrap()),
                );
            }
        }
    }
    let batch = |schema: &asap_physical_operators::values::Schema, name: &str| {
        let rows = samples
            .iter()
            .filter(|sample| sample.0 == name)
            .map(|(metric, job, seconds, value)| {
                let labels = BTreeMap::from([
                    ("__name__".to_string(), metric.to_string()),
                    ("job".to_string(), job.to_string()),
                ]);
                promql_rows::series_row(schema, &labels, seconds * 1000, *value).unwrap()
            })
            .collect();
        Batch::try_new(schema.clone(), rows).unwrap()
    };
    let frontier = frontier_from_timing(dag.as_view()).unwrap();
    let candidate = compile_candidate(
        dag,
        raw.iter()
            .map(|(id, (schema, _))| (*id, InputContract::bounded(schema.clone())))
            .collect(),
        &[u64::from(dag.root.0)],
        &frontier,
    )
    .unwrap();
    let raw_sources = |plan: &asap_physical_operators::physical_planner::PhysicalDAG| {
        plan.input_contracts()
            .filter_map(|(id, _)| raw.get(&id).map(|(schema, name)| (id, batch(schema, name))))
            .collect::<BTreeMap<_, _>>()
    };
    let mut query_sources = raw_sources(&candidate.query);
    if let Some(precompute) = &candidate.precompute {
        let stored = physical_common::execute(
            precompute,
            raw_sources(precompute),
            Scope::Ingestion {
                window_start_ms: 240_000,
                window_end_ms: 300_000,
                revision: 1,
            },
        );
        for (root, batches) in precompute.roots().iter().zip(stored) {
            query_sources.insert(*root, batches[0].clone());
        }
    }
    let result = physical_common::execute(
        &candidate.query,
        query_sources,
        Scope::Query {
            evaluation_time_ms: 300_000,
            revision: 1,
        },
    );
    result[0]
        .iter()
        .flat_map(|batch| batch.rows())
        .flat_map(|row| row.iter())
        .filter_map(|value| match value {
            Value::Float64(value) => Some(*value),
            _ => None,
        })
        .collect()
}

/// Prometheus drops series without a match: arithmetic over different
/// selectors keeps only label sets present on both sides (none when disjoint),
/// and such arithmetic never becomes aligned maintenance.
#[test]
fn maintained_arithmetic_over_different_selectors_matches_prometheus() {
    let query = "sum(sum_over_time(m[1m]) + sum_over_time(n[1m]))";
    let (dag, ingestion_binary) = typed_selection(query);
    let disjoint = [
        ("m", "a", 250, 1.0),
        ("m", "a", 290, 2.0),
        ("n", "b", 250, 5.0),
    ];
    let values = execute_timed(&dag, &disjoint);
    assert!(values.is_empty(), "{values:?}");
    // Only job a is on both sides: m_a + n_a = (1 + 2) + 7; m{job="b"} is dropped.
    let overlapping = [
        ("m", "a", 250, 1.0),
        ("m", "a", 290, 2.0),
        ("m", "b", 250, 5.0),
        ("n", "a", 250, 7.0),
    ];
    assert_eq!(execute_timed(&dag, &overlapping), [10.0]);
    assert!(!ingestion_binary);
    // The quantile's exact fallback runs outside Planner; it must not be maintained either.
    let (_, ingestion_binary) =
        typed_selection("quantile(0.9, sum_over_time(m[1m]) + sum_over_time(n[1m]))");
    assert!(!ingestion_binary);
}

/// Arithmetic over one selector keeps its maintained layout and adds each
/// series' two readouts before the quantile.
#[test]
fn maintained_arithmetic_over_one_selector_executes() {
    let (dag, ingestion_binary) =
        typed_selection("quantile(0.9, sum_over_time(m[1m]) + sum_over_time(m[1m]))");
    assert!(ingestion_binary, "one selector shares its key set");
    let values = execute_timed(
        &dag,
        &[
            ("m", "a", 250, 1.0),
            ("m", "a", 290, 2.0),
            ("m", "b", 250, 5.0),
        ],
    );
    // job a: 3 + 3 = 6; job b: 5 + 5 = 10 (mispairing a with b gives 8 and 8).
    // KLL at epsilon 0.01 returns an input value within 0.01 of rank 0.9; of
    // two values only the larger is.
    assert_eq!(values, [10.0]);
}

/// The named layer collections connect directly: IDs, rejections, lifecycle
/// metadata and shared logical identity survive through physical compilation.
#[test]
fn named_candidate_collections_preserve_timing_and_compile_errors() {
    use asap_aware_mapping::{
        CandidatePostASAPDAGsWithTiming, CandidateTimingContext, CandidateTimingError,
    };
    use asap_physical_operators::physical_planner::{
        compile_physical_dag_candidates, PhysicalCandidateError,
    };
    let workload = quantile_workload("sum by(job)(rate(m[1m]))");
    let lowered = lower_promql_workload(&workload, 0).unwrap().remove(0);
    let lowered =
        asap_physical_operators::physical_planner::promql_rows::with_series_identity(&lowered)
            .unwrap();
    let logical = asap_aware_mapping::search_workload(vec![(17usize, Rc::new(lowered))]);
    let context = || CandidateTimingContext {
        demand: WorkloadDemand::new_with_data(
            &workload.query_workload,
            workload.data_workload.as_ref().unwrap(),
            &[1],
        ),
        now_ms: NOW_MS,
        horizon: Some(Horizon(100.)),
        capabilities: SummaryMaintenanceLifecycleCapabilities::ALL,
        cost_model: &FullyCostedRuntime,
    };
    assert!(matches!(
        logical.with_timing_for_root(&17, context(), 4096, 0),
        Err(CandidateTimingError::ExpansionLimit(0))
    ));
    assert!(logical
        .with_timing_for_root(&18, context(), 4096, 4096)
        .is_err());
    let timed = logical
        .with_timing_for_root(&17, context(), 4096, 65536)
        .unwrap();
    assert!(!timed.is_empty());
    let mut indices = std::collections::BTreeMap::new();
    let mut valid = 0;
    let mut rejected = 0;
    for (metadata, assignment) in timed.iter() {
        assert_eq!(metadata.id, 17);
        match assignment {
            Ok(assignment) => {
                valid += 1;
                let plan = metadata.lifecycle.as_ref().unwrap();
                assert!(Rc::ptr_eq(
                    &plan.root,
                    assignment
                        .index()
                        .node_ids
                        .summary_node(assignment.index().root_id)
                        .unwrap()
                ));
                if let Some(previous) =
                    indices.insert(metadata.logical_candidate, assignment.index().clone())
                {
                    assert!(Rc::ptr_eq(&previous, assignment.index()));
                }
            }
            Err(_) => rejected += 1,
        }
    }
    assert!(valid > 1);
    assert!(rejected > 0);
    assert_eq!(valid + rejected, timed.len());
    // The total assignment budget is exact: the collection's size fits, one less does not.
    assert_eq!(
        logical
            .with_timing_for_root(&17, context(), 4096, timed.len())
            .unwrap()
            .len(),
        timed.len()
    );
    let short = timed.len() - 1;
    assert!(matches!(
        logical.with_timing_for_root(&17, context(), 4096, short),
        Err(CandidateTimingError::ExpansionLimit(limit)) if limit == short
    ));
    let physical = compile_physical_dag_candidates(
        timed.iter(),
        timed.rejected_assemblies().to_vec(),
        |_, assignment| {
            Ok((
                raw_inputs(&assignment.to_transport()),
                vec![u64::from(assignment.index().root_id.0)],
            ))
        },
    );
    assert_eq!(physical.len(), timed.len());
    assert_eq!(physical.rejected_assemblies(), timed.rejected_assemblies());
    let mut compiled = 0;
    for (i, ((before, timing), (after, dag))) in timed.iter().zip(physical.iter()).enumerate() {
        assert_eq!(
            (
                before.id,
                before.logical_candidate,
                before.assignment_candidate
            ),
            (
                after.id,
                after.logical_candidate,
                after.assignment_candidate
            )
        );
        assert_eq!(before.choices, after.choices);
        if timing.is_err() {
            assert!(matches!(dag, Err(PhysicalCandidateError::Timing(_))));
        }
        if dag.is_ok() {
            compiled += 1;
            assert!(after.lifecycle.is_some());
            physical.materialize(i).unwrap().validate().unwrap();
        }
    }
    assert!(compiled > 1);
    let failed = compile_physical_dag_candidates(timed.iter(), Vec::new(), |_, _| {
        Err(asap_physical_operators::Error::Invalid(
            "missing deployment input evidence".into(),
        ))
    });
    assert_eq!(failed.len(), timed.len());
    assert!(failed
        .iter()
        .all(|(metadata, result)| metadata.id == 17 && result.is_err()));
    assert!(physical.materialize(physical.len()).is_err());

    // The single-DAG entry point is the same collection, not a public lifecycle helper.
    let first = timed
        .iter()
        .find_map(|(metadata, timing)| {
            timing.ok().and_then(|_| {
                metadata
                    .lifecycle
                    .filter(|plan| !plan.deployments.is_empty())
                    .map(|plan| plan.root)
            })
        })
        .unwrap();
    let one = CandidatePostASAPDAGsWithTiming::from_post_asap_dag(
        17usize,
        first.clone(),
        context(),
        4096,
    )
    .unwrap();
    assert_eq!(one.logical_len(), 1);
    assert!(!one.lifecycle_alternatives(0).unwrap().is_empty());
    let mut invalid = context();
    invalid.horizon = Some(Horizon(-1.));
    let rejected =
        CandidatePostASAPDAGsWithTiming::from_post_asap_dag(17usize, first, invalid, 4096).unwrap();
    assert_eq!(rejected.len(), 1);
    let physical = compile_physical_dag_candidates(rejected.iter(), Vec::new(), |_, _| {
        panic!("invalid lifecycle context must not reach compilation")
    });
    assert_eq!(physical.len(), 1);
    let (metadata, error) = physical.iter().next().unwrap();
    assert_eq!(metadata.id, 17);
    assert!(matches!(
        error,
        Err(PhysicalCandidateError::Timing(timing))
            if matches!(timing.as_ref(), CandidateTimingError::Lifecycle(_))
    ));
    let mut counts = std::collections::BTreeMap::<usize, usize>::new();
    for (metadata, _) in timed.iter() {
        *counts.entry(metadata.logical_candidate).or_default() += 1;
    }
    let largest = *counts.values().max().unwrap();
    assert!(largest < timed.len());
    assert!(matches!(
        logical.with_timing_for_root(&17, context(), 4096, largest),
        Err(CandidateTimingError::ExpansionLimit(limit)) if limit == largest
    ));
}
