#![allow(dead_code)]

use asap_executor::runtime::{RunContext, Scope};
use asap_executor::values::{Batch, SchemaRef};
use futures::TryStreamExt;
use planner_types::ir::{OperatorNode, QueryRoot};
use std::rc::Rc;

/// The logical DAG the planner pipeline (`asap_planner::run_planning`) selects for the
/// single query `root` at `accuracy`, over a continuously ingested source,
/// with the built-in models.
pub fn selected_dag(
    root: Rc<OperatorNode>,
    accuracy: planner_types::accuracy::AccuracyTarget,
) -> Rc<OperatorNode> {
    let run = run_all_stages(root, accuracy);
    let roots = match &run.selection {
        Ok((plan, _)) => plan.query_roots().unwrap(),
        Err(error) => panic!("a plan is selected: {error}"),
    };
    let [QueryRoot::Operator(selected)] = roots.as_slice() else {
        panic!("one operator root");
    };
    Rc::clone(selected)
}

fn run_all_stages(
    root: Rc<OperatorNode>,
    accuracy: planner_types::accuracy::AccuracyTarget,
) -> asap_planner::PlanningRun {
    use planner_types::workload::{DataArrival, DataWorkload};
    let data = DataWorkload {
        arrival: DataArrival::Ingesting {
            rows_per_second: 1_000.0,
            max_out_of_order_ms: None,
        },
        ..Default::default()
    };
    asap_planner::run_planning(
        vec![QueryRoot::Operator(root)],
        &[planner_types::workload::QueryWorkloadEntry {
            query: Default::default(),
            requirements: planner_types::workload::QueryRequirements {
                accuracy,
                max_latency_ms: None,
            },
            predictability: Default::default(),
            recurrence: Default::default(),
            range_evaluation: None,
        }],
        &data,
        asap_planner::PlanningModels::builtin(),
        asap_planner::PlanSearchSettings::default(),
    )
    .expect("Pass 1 accepts the query")
}

/// Every root's batches in one run of `dag`, in `roots` order: through
/// `execute` at query time (each root drained in turn) and
/// `execute_ingestion` at ingestion time.
pub fn run_roots(
    dag: &asap_executor::plan::PhysicalDAG<'_, Batch, SchemaRef>,
    roots: &[u64],
    context: RunContext,
) -> Result<Vec<Vec<Batch>>, asap_executor::Error> {
    futures::executor::block_on(async {
        let outputs = match context.scope {
            Scope::Ingestion { .. } => dag.execute_ingestion(roots, context)?.await?,
            Scope::Query { .. } => {
                let mut outputs = Vec::new();
                for output in dag.execute(roots, context)? {
                    outputs.push(output.try_collect::<Vec<_>>().await?);
                }
                outputs
            }
        };
        Ok(outputs
            .into_iter()
            .map(|batches| batches.iter().map(|batch| (**batch).clone()).collect())
            .collect())
    })
}

/// Run `operator` over sources holding `inputs` (each its schema and rows)
/// at query time 0: the output rows, or the run's error. The run releases
/// every byte it charged.
pub fn run_over_sources(
    inputs: Vec<(SchemaRef, Vec<Vec<asap_executor::values::Value>>)>,
    operator: asap_executor::operators::Operator,
) -> Result<Vec<Vec<asap_executor::values::Value>>, asap_executor::Error> {
    run_over_sources_within(Default::default(), inputs, operator)
}

/// [`run_over_sources`] within `limits`.
pub fn run_over_sources_within(
    limits: asap_executor::runtime::Limits,
    inputs: Vec<(SchemaRef, Vec<Vec<asap_executor::values::Value>>)>,
    operator: asap_executor::operators::Operator,
) -> Result<Vec<Vec<asap_executor::values::Value>>, asap_executor::Error> {
    use asap_executor::operators::Operator;
    use asap_executor::plan::PhysicalDAG;
    use planner_types::stages::Lifetime;
    let mut dag = PhysicalDAG::default();
    let count = inputs.len() as u64;
    for (id, (schema, rows)) in inputs.into_iter().enumerate() {
        let batch = Batch::try_new(schema.clone(), rows).unwrap();
        dag.add(
            id as u64,
            vec![],
            Operator::source(schema, vec![batch]).unwrap(),
            Lifetime::None,
        )
        .unwrap();
    }
    dag.add(count, (0..count).collect(), operator, Lifetime::None)
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
        },
        limits,
    )
    .unwrap();
    let rows = run_roots(&dag, &[count], context.clone()).map(|mut outputs| {
        outputs
            .remove(0)
            .iter()
            .flat_map(Batch::to_rows)
            .collect::<Vec<_>>()
    });
    assert_eq!(context.retained_bytes(), 0);
    rows
}

/// A Planner node of `payload` typed `output`, for `compile_node`.
pub fn planner_node(
    payload: planner_types::ir::NonASAPOp<
        planner_types::plan::physical_asap_dag::PhysicalASAPNodeId,
    >,
    output: &SchemaRef,
) -> planner_types::plan::physical_asap_dag::PhysicalASAPDAGNode {
    planner_types::plan::physical_asap_dag::PhysicalASAPDAGNode {
        id: 99,
        output_schema: (**output).clone(),
        output_state: planner_types::ir::properties::ExecutionDataState::QUERY_ROWS,
        lifetime: Default::default(),
        parameters: None,
        retention_ms: None,
        payload: planner_types::plan::physical_asap_dag::PhysicalASAPOperatorPayload::NonASAP(
            payload,
        ),
        selected_physical_candidate: None,
        view_coverage: None,
    }
}

/// A PromQL vector's rows: the full label set (`labels`, a string map) and the
/// sample `value`, as the executor's vector operators read and write them.
pub fn promql_vector_schema() -> SchemaRef {
    use planner_types::ir::schema::{DataType, Field, Schema};
    let labels = DataType::Map {
        key: Box::new(DataType::Utf8),
        value: Box::new(DataType::Utf8),
        value_nullable: false,
    };
    std::sync::Arc::new(Schema::new(vec![
        Field::plain("labels", labels, false),
        Field::plain("value", DataType::Float64, false),
    ]))
}

/// A PromQL scalar: one `$promql_scalar` value and no labels.
pub fn promql_scalar_schema() -> SchemaRef {
    use planner_types::ir::schema::{DataType, Field, Schema};
    std::sync::Arc::new(Schema::new(vec![Field::plain(
        "$promql_scalar",
        DataType::Float64,
        false,
    )]))
}

/// A PromQL range vector's samples: [`promql_vector_schema`] with each
/// sample's `timestamp` (the time index) and the `window_start` and
/// `window_end` of the range it belongs to.
pub fn promql_matrix_schema() -> SchemaRef {
    use planner_types::ir::schema::{DataType, Field, Schema};
    let mut fields = promql_vector_schema().fields.clone();
    fields.insert(1, Field::plain("timestamp", DataType::Timestamp, false));
    fields.push(Field::plain("window_start", DataType::Timestamp, false));
    fields.push(Field::plain("window_end", DataType::Timestamp, false));
    std::sync::Arc::new(Schema {
        fields,
        unique_keys: vec![],
        closed: false,
        time_index: Some(1),
    })
}
