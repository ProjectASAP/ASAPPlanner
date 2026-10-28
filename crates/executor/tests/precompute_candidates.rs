//! A grouped Rate→Sum answers the same whether the deployment stores the
//! Rate state or the grouped Sum state.
use asap_executor::sources::InMemoryDataStore;
mod common;
use asap_executor::{
    factory::create_planner_accumulator,
    physical_planner::{compile, test_support::views_at, CompiledPhysicalDAG},
    runtime::{Limits, RunContext, Scope},
    values::{Batch, Value},
};
use common::{compile_physical_asap_dag, computation_candidates, selected_dag};
use futures::{executor::block_on, StreamExt};
use planner_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPOperatorPayload};
use planner_types::ir::properties::AccuracyTarget;
use planner_types::ir::schema::{DataType, *};
use planner_types::ir::ASAPOp;
use planner_types::workload::*;
use std::{collections::BTreeMap, sync::Arc};

/// `sum by(job)(rate(m[1m]))` with its rows carrying the series identity.
fn grouped_rate_root() -> std::rc::Rc<planner_types::ir::OperatorNode> {
    exact_promql_root("sum by(job)(rate(m[1m]))")
}

/// `query`, exact, lowered with its rows carrying the series identity.
fn exact_promql_root(query: &str) -> std::rc::Rc<planner_types::ir::OperatorNode> {
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: Some(vec![BatchEntry {
                query: Query(query.into()),
                requirements: QueryRequirements {
                    accuracy: AccuracyTarget::exact(),
                    ..Default::default()
                },
                predictability: Predictability::Unknown,
                invocations: 1,
                execute_at: None,
                range_evaluation: None,
            }]),
            repeating_queries: None,
        },
        data_workload: Some(DataWorkload::sampled_every(
            asap_frontend_promql::promql_metric_names(query).unwrap_or_default(),
            DurationMs(1000),
        )),
    };
    let root = asap_frontend_promql::lower_promql_workload(&workload)
        .unwrap()
        .remove(0);
    asap_executor::physical_planner::promql_rows::with_series_identity(&root).unwrap()
}

fn grouped_rate() -> PhysicalASAPDAG {
    compile_physical_asap_dag(&selected_dag(grouped_rate_root(), AccuracyTarget::exact())).unwrap()
}
fn run(plan: &CompiledPhysicalDAG, store: InMemoryDataStore, scope: Scope) -> Vec<Batch> {
    let dag = plan.instantiate(Arc::new(store)).unwrap();
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

/// The root's output when the deployment stores the Rate `state` (bound to
/// `states`) and, with `stored`, also the output of node `stored`, which an
/// ingestion run over `ingestion` computes from the states.
fn answer(
    dag: &PhysicalASAPDAG,
    state: u64,
    states: &Batch,
    stored: Option<u64>,
    ingestion: Scope,
    query: Scope,
) -> Vec<Batch> {
    let mut dag = dag.clone();
    let views = views_at(
        &mut dag,
        &[state].into_iter().chain(stored).collect::<Vec<_>>(),
    )
    .unwrap();
    let state_store = || {
        InMemoryDataStore::default()
            .with_materialized_view(state as usize, vec![states.clone()])
            .unwrap()
    };
    let root = [dag.roots[0] as u64];
    let Some(stored) = stored else {
        return run(&compile(&dag, &views, &root).unwrap(), state_store(), query);
    };
    let outputs = run(
        &compile(&dag, &views[..1], &[stored]).unwrap(),
        state_store(),
        ingestion,
    );
    assert_eq!(outputs.len(), 1);
    let store = InMemoryDataStore::default()
        .with_materialized_view(stored as usize, outputs)
        .unwrap();
    run(&compile(&dag, &views[1..], &root).unwrap(), store, query)
}

/// Rate evaluations and grouped Sum can run together at ingestion into a
/// stored Sum state, which answers as the query-time Rate→Sum over the stored
/// Rate state. Moving Sum before Rate would answer differently across
/// counter resets.
#[test]
fn grouped_rate_can_be_materialized_before_or_after_grouped_sum() {
    let dag = grouped_rate();
    let state = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
                    ..
                })
            )
        })
        .unwrap();
    let input_schema = Arc::new(state.output_schema.clone());
    let (family, update, grouping) = match &state.payload {
        PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
            family,
            input,
            grouping,
            ..
        }) => (family, input, grouping),
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
                accumulator.update_single(value, i as i64 * 1000).unwrap();
            }
            let state = accumulator.into_accumulator();
            expected_rate_sum += state
                .as_any()
                .downcast_ref::<asap_executor::summary_kernels::exact::ExactAccumulator>()
                .unwrap()
                .evaluation(ExactKind::Rate, range_ms, None)
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
                    FieldDataType::ExactAggregate(..) => summary.clone(),
                    FieldDataType::Plain(DataType::Timestamp) => Value::Timestamp(2000),
                    FieldDataType::Plain(DataType::Utf8) => Value::Utf8(if field.name == "job" {
                        "api".into()
                    } else {
                        serde_json::to_string(&BTreeMap::from([
                            ("__name__", "m".to_string()),
                            ("job", "api".to_string()),
                            ("instance", format!("series-{index}")),
                        ]))
                        .unwrap()
                        .into()
                    }),
                    _ => panic!("unexpected input field {field:?}"),
                })
                .collect()
        })
        .collect();
    let batch = Batch::try_new(input_schema.clone(), rows).unwrap();
    let state_id = state.id as u64;
    let sum_id = dag
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                    ..
                })
            )
        })
        .unwrap()
        .id as u64;
    let ingestion = Scope::Ingestion {
        window_start_ms: -58_000,
        window_end_ms: 2000,
    };
    let query = Scope::Query {
        evaluation_time_ms: 2000,
    };
    let mut results = vec![];
    for stored in [None, Some(sum_id)] {
        let output = answer(
            &dag,
            state_id,
            &batch,
            stored,
            ingestion.clone(),
            query.clone(),
        );
        assert_eq!(output.len(), 1, "{stored:?}");
        assert_eq!(output[0].to_rows().len(), 1);
        assert!(matches!(&output[0].to_rows()[0][0], Value::Utf8(job) if job.as_ref() == "api"));
        assert!(
            matches!(output[0].to_rows()[0][1], Value::Float64(value) if value == expected_rate_sum)
        );
        results.push(
            output[0].to_rows()[0]
                .iter()
                .map(|value| value.key().unwrap())
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(results[0], results[1]);
    let mut wrong_order = create_planner_accumulator(family, update, grouping).unwrap();
    for (i, value) in [200., 200., 100.].into_iter().enumerate() {
        wrong_order.update_single(value, i as i64 * 1000).unwrap();
    }
    let rate_of_sum = wrong_order
        .into_accumulator()
        .as_any()
        .downcast_ref::<asap_executor::summary_kernels::exact::ExactAccumulator>()
        .unwrap()
        .evaluation(ExactKind::Rate, range_ms, None)
        .unwrap()
        .unwrap();
    assert_ne!(
        expected_rate_sum, rate_of_sum,
        "counter resets prohibit moving Sum before Rate"
    );
}

/// Every Pass 1 computation candidate of the grouped Rate answers a numeric
/// value at query time, from the stored Rate state or a stored grouped Sum.
#[test]
fn enumerated_grouped_rate_candidates_execute_numeric_query_outputs() {
    let mut executed = 0;
    for root in computation_candidates(grouped_rate_root()) {
        let dag = compile_physical_asap_dag(&root).unwrap();
        let Some(state) = dag.nodes.iter().find(|node| {
            matches!(
                node.payload,
                PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::ExactAggregate(ExactKind::Rate, _),
                    ..
                })
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
                    PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::ExactAggregate(ExactKind::Sum, _),
                        ..
                    })
                )
            })
            .map(|node| node.id as u64)
            .unwrap_or(dag.roots[0] as u64);
        let (family, input, grouping) = match &state.payload {
            PhysicalASAPOperatorPayload::ASAP(ASAPOp::SummaryAgg {
                family,
                input,
                grouping,
                ..
            }) => (family, input, grouping),
            _ => unreachable!(),
        };
        let schema = Arc::new(state.output_schema.clone());
        let rows = ["a", "b"]
            .into_iter()
            .map(|instance| {
                let mut accumulator = create_planner_accumulator(family, input, grouping).unwrap();
                for (timestamp, value) in [(1_000, 1.), (31_000, 31.), (59_000, 59.)] {
                    accumulator.update_single(value, timestamp).unwrap();
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
                        FieldDataType::Plain(DataType::Timestamp) => Value::Timestamp(60_000),
                        FieldDataType::Plain(DataType::Utf8)
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
                        FieldDataType::Plain(DataType::Utf8) => Value::Utf8("api".into()),
                        _ => panic!("unexpected input field {field:?}"),
                    })
                    .collect()
            })
            .collect();
        let batch = Batch::try_new(schema, rows).unwrap();
        for stored in [None, Some(boundary)] {
            let output = answer(
                &dag,
                state.id as u64,
                &batch,
                stored,
                Scope::Ingestion {
                    window_start_ms: 0,
                    window_end_ms: 60_000,
                },
                Scope::Query {
                    evaluation_time_ms: 60_000,
                },
            );
            assert_eq!(output.len(), 1);
            assert_eq!(output[0].to_rows().len(), 1);
            assert!(output[0]
                .schema()
                .fields
                .iter()
                .all(|field| matches!(field.dtype, FieldDataType::Plain(_))));
            assert!(
                output[0].to_rows()[0]
                    .iter()
                    .any(|value| matches!(value, Value::Float64(x) if (*x - 2.).abs() < 1e-12)),
                "{:?}",
                output[0].to_rows()
            );
            executed += 1;
        }
    }
    assert!(
        executed >= 2,
        "must execute both stored and query-time grouped Rate candidates: {executed}"
    );
}
