//! Materialized frontiers are compiled by Planner, never rewritten by deployment.
use asap_aware_mapping::{cost_model::DefaultCostModel, search_workload};
use asap_physical_operators::{
    factory::create_planner_accumulator,
    operators::Operator,
    physical_planner::{
        compile_candidates, select_candidate, CandidateCost, CompiledPhysicalDag, InputContract,
        Source,
    },
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use futures::{executor::block_on, StreamExt};
use planner_types::{post_asap::*, pre_asap::DataType, types::AccuracyTarget, workload::*};
use std::{collections::BTreeMap, rc::Rc, sync::Arc};

fn grouped_rate_space() -> asap_aware_mapping::PlanSpace<&'static str> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query("sum by(job)(rate(m[1m]))".into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::Exact),
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
            data_ingestion_interval: Evidence {
                value: Some(DurationMs(1000)),
                ..Default::default()
            },
            ..Default::default()
        }),
    };
    let root = Rc::new(
        asap_frontend_promql::lower_promql_workload(&workload, 0)
            .unwrap()
            .remove(0),
    );
    let root = Rc::new(
        asap_physical_operators::physical_planner::promql_rows::with_series_identity(&root)
            .unwrap(),
    );
    search_workload(vec![("grouped-rate", root)])
}

fn grouped_rate() -> PostAsapDag {
    let space = grouped_rate_space();
    let selected = space
        .global_selection(&DefaultCostModel)
        .assemble_selected_dag(&space.roots[0].1)
        .unwrap()
        .unwrap();
    compile_post_asap_dag(&selected).unwrap()
}
fn run(plan: &CompiledPhysicalDag, inputs: BTreeMap<u64, Batch>, scope: Scope) -> Vec<Batch> {
    let sources = inputs
        .into_iter()
        .map(|(id, batch)| {
            let source = Operator::source(batch.schema().clone(), vec![batch]).unwrap();
            (id, Box::new(source) as Source<'_>)
        })
        .collect();
    let dag = plan.instantiate(sources).unwrap();
    block_on(async {
        let context = RunContext::new(scope, Limits::default()).unwrap();
        let mut output = dag.execute(plan.roots(), context).unwrap().remove(0);
        let mut batches = vec![];
        while let Some(batch) = output.next().await {
            batches.push((*batch.unwrap()).clone());
        }
        batches
    })
}

/// Rate readouts and grouped Sum can run together during bounded precompute;
/// storing per-series rates instead leaves the same Sum in the query DAG.
#[test]
fn grouped_rate_can_be_materialized_before_or_after_grouped_sum() {
    let dag = grouped_rate();
    let state = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PostAsapOperatorPayload::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Rate, _),
                    ..
                }
            )
        })
        .unwrap();
    let readout = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PostAsapOperatorPayload::Value {
                    operation: ValueOperation::FinalizeExactAccumulator
                }
            )
        })
        .unwrap();
    let input_schema = Arc::new(state.output_schema.clone());
    let (family, update, grouping) = match &state.payload {
        PostAsapOperatorPayload::SummaryAgg {
            family,
            input,
            grouping,
            ..
        } => (family, input, grouping),
        _ => unreachable!(),
    };
    let range_ms = Some((-58_000, 2_000));
    let mut expected_rate_sum = 0.;
    let rows = [[100., 0., 100.], [100., 200., 0.]]
        .into_iter()
        .enumerate()
        .map(|(index, values)| {
            let mut accumulator = create_planner_accumulator(family, update, grouping).unwrap();
            for (i, value) in values.into_iter().enumerate() {
                accumulator.update_single(value, i as i64 * 1000);
            }
            let state = accumulator.into_accumulator();
            expected_rate_sum += state
                .as_any()
                .downcast_ref::<asap_physical_operators::summary_kernels::exact::ExactAccumulator>()
                .unwrap()
                .readout(asap_physical_operators::Statistic::Rate, range_ms, None)
                .unwrap()
                .unwrap();
            let summary = Value::Summary {
                family: family.clone(),
                state: Arc::from(state),
            };
            input_schema
                .fields
                .iter()
                .map(|field| match &field.dtype {
                    SummaryFamilyType::ExactAggregate(..) => summary.clone(),
                    SummaryFamilyType::Plain(DataType::Timestamp) => Value::Timestamp(2000),
                    SummaryFamilyType::Plain(DataType::Utf8) => {
                        Value::Utf8(if field.name == "job" {
                            "api".into()
                        } else {
                            format!("series-{index}").into()
                        })
                    }
                    _ => panic!("unexpected input field {field:?}"),
                })
                .collect()
        })
        .collect();
    let batch = Batch::try_new(input_schema.clone(), rows).unwrap();
    let root = u64::from(dag.root.0);
    let state_id = u64::from(state.id.0);
    let rate_id = u64::from(readout.id.0);
    let frontiers = asap_physical_operators::physical_planner::enumerate_frontiers(
        &dag,
        &BTreeMap::from([(state_id, InputContract::bounded(input_schema.clone()))]),
        &[root],
        128,
    )
    .unwrap();
    assert!(frontiers.contains(&vec![]));
    assert!(frontiers.contains(&vec![rate_id]));
    assert!(frontiers.contains(&vec![root]));
    assert!(!frontiers.contains(&vec![root, rate_id]));
    assert!(
        asap_physical_operators::physical_planner::enumerate_frontiers(
            &dag,
            &BTreeMap::from([(state_id, InputContract::bounded(input_schema.clone()))]),
            &[root],
            1,
        )
        .is_err()
    );
    let candidates = compile_candidates(
        &dag,
        BTreeMap::from([(state_id, InputContract::bounded(input_schema))]),
        &[root],
        &[vec![], vec![rate_id], vec![root]],
    );
    // Scoped cost fixtures select either precompute boundary. No readers are
    // opened during candidate construction or selection.
    for prefer_grouped in [false, true] {
        let inventory = compile_candidates(
            &dag,
            BTreeMap::from([(
                state_id,
                InputContract::bounded(Arc::new(state.output_schema.clone())),
            )]),
            &[root],
            &[vec![999], vec![rate_id], vec![root]],
        );
        assert!(inventory[0].is_err());
        let mut evaluated = 0;
        let selected = select_candidate(inventory, |candidate| {
            evaluated += 1;
            let grouped = candidate.materialized_outputs.contains_key(&root);
            Ok(Some(CandidateCost {
                workload_scope: "reset-counter-workload".into(),
                horizon_seconds: 300.,
                total_cost: if grouped == prefer_grouped { 1. } else { 100. },
            }))
        })
        .unwrap();
        assert_eq!(
            selected.candidate.materialized_outputs.contains_key(&root),
            prefer_grouped
        );
        assert_eq!(selected.cost.total_cost, 1.);
        assert_eq!(evaluated, 2, "uncompilable candidates must never be priced");
        let candidate = selected.candidate;
        let precompute = candidate.precompute.as_ref().unwrap();
        let stored = run(
            precompute,
            BTreeMap::from([(state_id, batch.clone())]),
            Scope::Ingestion {
                window_start_ms: -58_000,
                window_end_ms: 2000,
                revision: 1,
            },
        );
        let output = run(
            &candidate.query,
            BTreeMap::from([(precompute.roots()[0], stored[0].clone())]),
            Scope::Query {
                evaluation_time_ms: 2000,
                revision: 1,
            },
        );
        assert!(
            matches!(output[0].rows()[0][1], Value::Float64(value) if value == expected_rate_sum)
        );
    }
    let contracts = BTreeMap::from([(
        state_id,
        InputContract::bounded(Arc::new(state.output_schema.clone())),
    )]);
    for frontier in [vec![rate_id, rate_id], vec![root, rate_id], vec![999]] {
        assert!(
            asap_physical_operators::physical_planner::compile_candidate(
                &dag,
                contracts.clone(),
                &[root],
                &frontier
            )
            .is_err()
        );
    }
    let inventory = compile_candidates(
        &dag,
        contracts.clone(),
        &[root],
        &[vec![rate_id], vec![root]],
    );
    let selected = select_candidate(inventory, |candidate| {
        if candidate.materialized_outputs.contains_key(&root) {
            return Ok(None);
        }
        Ok(Some(CandidateCost {
            workload_scope: "same-workload".into(),
            horizon_seconds: 300.,
            total_cost: 100.,
        }))
    })
    .unwrap();
    assert!(selected
        .candidate
        .materialized_outputs
        .contains_key(&rate_id));
    let inventory = compile_candidates(&dag, contracts, &[root], &[vec![rate_id], vec![root]]);
    assert!(
        select_candidate(inventory, |candidate| Ok(Some(CandidateCost {
            workload_scope: "same-workload".into(),
            horizon_seconds: if candidate.materialized_outputs.contains_key(&root) {
                60.
            } else {
                300.
            },
            total_cost: 1.,
        })))
        .is_err()
    );
    let query_scope = Scope::Query {
        evaluation_time_ms: 2000,
        revision: 1,
    };
    let maintenance_scope = Scope::Ingestion {
        window_start_ms: -58_000,
        window_end_ms: 2000,
        revision: 1,
    };
    let mut results = vec![];
    for candidate in candidates {
        let candidate = candidate.unwrap();
        let inputs = if let Some(precompute) = &candidate.precompute {
            let source = Operator::source(batch.schema().clone(), vec![batch.clone()]).unwrap();
            let invalid = precompute
                .instantiate(BTreeMap::from([(state_id, Box::new(source) as Source<'_>)]))
                .unwrap();
            let context = RunContext::new(
                Scope::Ingestion {
                    window_start_ms: 0,
                    window_end_ms: 2000,
                    revision: 1,
                },
                Limits::default(),
            )
            .unwrap();
            assert!(invalid.execute(precompute.roots(), context).is_err());
            let stored = run(
                precompute,
                BTreeMap::from([(state_id, batch.clone())]),
                maintenance_scope.clone(),
            );
            assert_eq!(stored.len(), 1);
            let boundary = precompute.roots()[0];
            assert_eq!(
                candidate.materialized_outputs[&boundary].schema,
                *stored[0].schema()
            );
            BTreeMap::from([(boundary, stored[0].clone())])
        } else {
            BTreeMap::from([(state_id, batch.clone())])
        };
        let output = run(&candidate.query, inputs, query_scope.clone());
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].rows().len(), 1);
        assert!(matches!(&output[0].rows()[0][0], Value::Utf8(job) if job.as_ref() == "api"));
        assert!(
            matches!(output[0].rows()[0][1], Value::Float64(value) if value == expected_rate_sum)
        );
        results.push(
            output[0].rows()[0]
                .iter()
                .map(|value| value.key().unwrap())
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(results[1], results[2]);
    let mut wrong_order = create_planner_accumulator(family, update, grouping).unwrap();
    for (i, value) in [200., 200., 100.].into_iter().enumerate() {
        wrong_order.update_single(value, i as i64 * 1000);
    }
    let rate_of_sum = wrong_order
        .into_accumulator()
        .as_any()
        .downcast_ref::<asap_physical_operators::summary_kernels::exact::ExactAccumulator>()
        .unwrap()
        .readout(asap_physical_operators::Statistic::Rate, range_ms, None)
        .unwrap()
        .unwrap();
    assert_ne!(
        expected_rate_sum, rate_of_sum,
        "counter resets prohibit moving Sum before Rate"
    );
}

/// Enumerated frontiers include both grouped-result and per-series readout
/// persistence; an explicit Rate-state input retains its original semantics.
#[test]
fn bounded_inventory_exposes_grouped_rate_physical_frontiers() {
    use asap_physical_operators::physical_planner::enumerate_frontiers;
    let dag = grouped_rate();
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
    let inputs = BTreeMap::from([(
        u64::from(state.id.0),
        InputContract::bounded(Arc::new(state.output_schema.clone())),
    )]);
    let roots = [u64::from(dag.root.0)];
    let frontiers = enumerate_frontiers(&dag, &inputs, &roots, 4096).unwrap();
    let candidates = compile_candidates(&dag, inputs.clone(), &roots, &frontiers)
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(candidates.iter().any(|c| c.precompute.is_none()));
    assert!(candidates
        .iter()
        .any(|c| c.materialized_outputs.contains_key(&roots[0])));
    assert!(candidates
        .iter()
        .any(|c| !c.materialized_outputs.is_empty()
            && !c.materialized_outputs.contains_key(&roots[0])));
    assert!(enumerate_frontiers(&dag, &inputs, &roots, 1).is_err());
}

#[test]
fn enumerated_grouped_rate_candidates_execute_numeric_query_outputs() {
    let inventory = grouped_rate_space().enumerate_candidate_dags(4096).unwrap();
    let mut executed = 0;
    for forest in inventory.candidates {
        let root = &forest[0].1;
        let dag = compile_post_asap_dag(root).unwrap();
        let Some(state) = dag.nodes.iter().find(|node| {
            matches!(
                node.payload,
                PostAsapOperatorPayload::SummaryAgg {
                    family: SummaryFamilyType::ExactAggregate(ExactKind::Rate, _),
                    ..
                }
            )
        }) else {
            continue;
        };
        let boundary = dag
            .nodes
            .iter()
            .find(|node| {
                matches!(
                    node.payload,
                    PostAsapOperatorPayload::SummaryAgg {
                        family: SummaryFamilyType::ExactAggregate(ExactKind::Sum, _),
                        ..
                    }
                )
            })
            .map(|node| u64::from(node.id.0))
            .unwrap_or(u64::from(dag.root.0));
        let physical_candidates = compile_candidates(
            &dag,
            BTreeMap::from([(
                u64::from(state.id.0),
                InputContract::bounded(Arc::new(state.output_schema.clone())),
            )]),
            &[u64::from(dag.root.0)],
            &[vec![], vec![boundary]],
        );
        let (family, input, grouping) = match &state.payload {
            PostAsapOperatorPayload::SummaryAgg {
                family,
                input,
                grouping,
                ..
            } => (family, input, grouping),
            _ => unreachable!(),
        };
        let schema = Arc::new(state.output_schema.clone());
        let rows = ["a", "b"]
            .into_iter()
            .map(|instance| {
                let mut accumulator = create_planner_accumulator(family, input, grouping).unwrap();
                for (timestamp, value) in [(1_000, 1.), (31_000, 31.), (59_000, 59.)] {
                    accumulator.update_single(value, timestamp);
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
                        SummaryFamilyType::Plain(DataType::Timestamp) => Value::Timestamp(60_000),
                        SummaryFamilyType::Plain(DataType::Utf8)
                            if field.name == "$promql_series_identity" =>
                        {
                            Value::Utf8(
                                serde_json::to_string(&BTreeMap::from([
                                    ("job", "api"),
                                    ("instance", instance),
                                ]))
                                .unwrap()
                                .into(),
                            )
                        }
                        SummaryFamilyType::Plain(DataType::Utf8) => Value::Utf8("api".into()),
                        _ => panic!("unexpected input field {field:?}"),
                    })
                    .collect()
            })
            .collect();
        let batch = Batch::try_new(schema, rows).unwrap();
        for physical in physical_candidates {
            let physical = physical.unwrap();
            let inputs = if let Some(precompute) = &physical.precompute {
                let source_id = precompute.input_contracts().next().unwrap().0;
                let stored = run(
                    precompute,
                    BTreeMap::from([(source_id, batch.clone())]),
                    Scope::Ingestion {
                        window_start_ms: 0,
                        window_end_ms: 60_000,
                        revision: 1,
                    },
                );
                assert_eq!(stored.len(), 1);
                BTreeMap::from([(precompute.roots()[0], stored[0].clone())])
            } else {
                BTreeMap::from([(
                    physical.query.input_contracts().next().unwrap().0,
                    batch.clone(),
                )])
            };
            let output = run(
                &physical.query,
                inputs,
                Scope::Query {
                    evaluation_time_ms: 60_000,
                    revision: 1,
                },
            );
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].rows().len(), 1);
            assert!(output[0]
                .schema()
                .fields
                .iter()
                .all(|field| matches!(field.dtype, SummaryFamilyType::Plain(_))));
            assert!(
                output[0].rows()[0]
                    .iter()
                    .any(|value| matches!(value, Value::Float64(x) if (*x - 2.).abs() < 1e-12)),
                "{:?}",
                output[0].rows()
            );
            executed += 1;
        }
    }
    assert!(
        executed >= 2,
        "must execute both stored and query-time grouped Rate candidates: {executed}"
    );
}
