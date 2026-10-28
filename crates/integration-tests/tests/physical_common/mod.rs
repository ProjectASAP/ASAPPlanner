use asap_executor::{
    physical_planner::CompiledPhysicalDAG,
    runtime::{Limits, RunContext, Scope},
    sources::InMemoryDataStore,
    values::Batch,
};
use futures::{executor::block_on, StreamExt};
use std::sync::Arc;

/// Run `plan` against `store` and collect each root's batches.
#[allow(dead_code)]
pub fn execute(
    plan: &CompiledPhysicalDAG,
    store: InMemoryDataStore,
    scope: Scope,
) -> Vec<Vec<Batch>> {
    let dag = plan.instantiate(Arc::new(store)).unwrap();
    block_on(async {
        let streams = dag
            .execute(
                plan.roots(),
                RunContext::new(scope, Limits::default()).unwrap(),
            )
            .unwrap();
        futures::future::join_all(streams.into_iter().map(|mut stream| async move {
            let mut batches = Vec::new();
            while let Some(batch) = stream.next().await {
                batches.push((*batch.unwrap()).clone());
            }
            batches
        }))
        .await
    })
}

#[allow(dead_code)]
pub fn compile_physical_asap_dag(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
) -> Result<asap_types::ir::physical_export::PhysicalASAPDAG, Box<dyn std::error::Error>> {
    compile_with(root, &asap_types::ir::MaterializationAssignment::default())
}

/// As [`compile_physical_asap_dag`], with every sub-DAG `MatAtIngestion`
/// where the DAG allows it.
#[allow(dead_code)] // Not every test binary sharing this module stores states at ingestion.
pub fn compile_at_ingestion_physical_asap_dag(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
) -> Result<asap_types::ir::physical_export::PhysicalASAPDAG, Box<dyn std::error::Error>> {
    compile_with(
        root,
        &asap_types::ir::MaterializationAssignment::with_default(
            asap_types::physical::Materialization::MatAtIngestion(
                asap_types::physical::Medium::Memory,
            ),
        ),
    )
}

fn compile_with(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
    assignment: &asap_types::ir::MaterializationAssignment,
) -> Result<asap_types::ir::physical_export::PhysicalASAPDAG, Box<dyn std::error::Error>> {
    let root =
        asap_types::ir::apply_materialization_timings(root, assignment, &mut Default::default())?;
    Ok(asap_types::ir::physical_export::compile_physical_asap_dag(
        &root,
    )?)
}

/// Execute a relational DAG at query time against an in-memory connector
/// for its one scan, scan predicates included; checks that no memory stays
/// reserved afterwards.
#[allow(dead_code)]
pub fn execute_raw_rows(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
    rows: Vec<Vec<asap_executor::values::Value>>,
) -> Vec<Vec<asap_executor::values::Value>> {
    execute_raw_rows_at(root, rows, 0)
}

/// [`execute_raw_rows`] evaluated at `evaluation_time_ms` (`now()`, and the
/// end of every trailing window).
#[allow(dead_code)]
pub fn execute_raw_rows_at(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
    rows: Vec<Vec<asap_executor::values::Value>>,
    evaluation_time_ms: i64,
) -> Vec<Vec<asap_executor::values::Value>> {
    use asap_executor::physical_planner::compile;
    use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
    use asap_types::ir::NonASAPOp;
    let wire = compile_physical_asap_dag(root).unwrap();
    let (source, schema) = wire
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) => {
                Some((source.clone(), node.output_schema.clone()))
            }
            _ => None,
        })
        .expect("one scan");
    let input = Arc::new(schema);
    let batch = Batch::try_new(input.clone(), rows).unwrap();
    let store = InMemoryDataStore::default()
        .with_raw_data(source, input, vec![batch])
        .unwrap();
    let root_id = wire.roots[0] as u64;
    let plan = compile(&wire, &[], &[root_id])
        .unwrap()
        .instantiate(Arc::new(store))
        .unwrap();
    let context = RunContext::new(Scope::Query { evaluation_time_ms }, Limits::default()).unwrap();
    let result = block_on(async {
        let mut output = plan.execute(&[root_id], context.clone()).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = output.next().await {
            rows.extend(batch.unwrap().to_rows());
        }
        rows
    });
    assert_eq!(context.retained_bytes(), 0);
    result
}

/// A store with raw data bound at every distinct `Scan` source of `dag`:
/// `rows(source, schema)` in that `Scan`'s schema.
#[allow(dead_code)]
pub fn raw_data_store(
    dag: &asap_types::ir::physical_export::PhysicalASAPDAG,
    mut rows: impl FnMut(
        &asap_types::ir::operator::Source,
        &asap_executor::values::SchemaRef,
    ) -> Vec<Vec<asap_executor::values::Value>>,
) -> InMemoryDataStore {
    use asap_types::ir::physical_export::PhysicalASAPOperatorPayload;
    use asap_types::ir::NonASAPOp;
    let mut store = InMemoryDataStore::default();
    let mut bound = Vec::new();
    for node in &dag.nodes {
        let PhysicalASAPOperatorPayload::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload
        else {
            continue;
        };
        if bound.contains(source) {
            continue;
        }
        bound.push(source.clone());
        let schema = Arc::new(node.output_schema.clone());
        let batch = Batch::try_new(schema.clone(), rows(source, &schema)).unwrap();
        store = store
            .with_raw_data(source.clone(), schema, vec![batch])
            .unwrap();
    }
    store
}
