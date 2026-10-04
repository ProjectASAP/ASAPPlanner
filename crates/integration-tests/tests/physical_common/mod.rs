use asap_executor::{
    operators::Operator,
    physical_planner::{CompiledPhysicalDAG, Source},
    runtime::{Limits, RunContext, Scope},
    values::Batch,
};
use futures::{executor::block_on, StreamExt};
use std::collections::BTreeMap;

#[allow(dead_code)]
pub fn execute(
    plan: &CompiledPhysicalDAG,
    inputs: BTreeMap<u64, Batch>,
    scope: Scope,
) -> Vec<Vec<Batch>> {
    let sources = inputs
        .into_iter()
        .map(|(id, batch)| {
            (
                id,
                Box::new(Operator::source(batch.schema().clone(), vec![batch]).unwrap())
                    as Source<'_>,
            )
        })
        .collect();
    let dag = plan.instantiate(sources).unwrap();
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
) -> Result<asap_types::ir::export::PhysicalASAPDAG, Box<dyn std::error::Error>> {
    compile_with(
        root,
        &asap_types::ir::MaterializationAssignment::all_query_time(),
    )
}

/// As [`compile_physical_asap_dag`], with every summary maintained at
/// ingestion time, the placement precompute compilation requires.
#[allow(dead_code)] // Not every test binary sharing this module compiles precompute.
pub fn compile_maintained_physical_asap_dag(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
) -> Result<asap_types::ir::export::PhysicalASAPDAG, Box<dyn std::error::Error>> {
    compile_with(
        root,
        &asap_types::ir::MaterializationAssignment::all_ingestion_time(),
    )
}

fn compile_with(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
    assignment: &asap_types::ir::MaterializationAssignment,
) -> Result<asap_types::ir::export::PhysicalASAPDAG, Box<dyn std::error::Error>> {
    let root =
        asap_types::ir::apply_materialization_timings(root, assignment, &mut Default::default())?;
    Ok(asap_types::ir::export::compile_physical_asap_dag(&root)?)
}

/// Execute a relational DAG against a raw in-memory connector for its one
/// scan, including scan predicates; checks that no memory stays reserved.
#[allow(dead_code)]
pub fn execute_raw_rows(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
    rows: Vec<Vec<asap_executor::values::Value>>,
) -> Vec<Vec<asap_executor::values::Value>> {
    use asap_executor::{
        physical_planner::bind_with_data_sources,
        sources::{DataSources, MemorySource},
    };
    use asap_types::ir::export::{NonASAPOpKind, PhysicalASAPOperatorPayload};
    use std::sync::Arc;
    let wire = compile_physical_asap_dag(root).unwrap();
    let (source, schema) = wire
        .nodes
        .iter()
        .find_map(|node| match &node.payload {
            PhysicalASAPOperatorPayload::Relational {
                operator: NonASAPOpKind::Scan { source, .. },
            } => Some((source.clone(), node.output_schema.clone())),
            _ => None,
        })
        .unwrap();
    let input = Arc::new(schema);
    let batch = Batch::try_new(input.clone(), rows).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            source,
            Arc::new(MemorySource::new(input, vec![batch]).unwrap()),
        )
        .unwrap();
    let root_id = u64::from(wire.roots[0].0);
    let plan = bind_with_data_sources(&wire, BTreeMap::new(), &[root_id], &sources).unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms: 0,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
    let result = block_on(async {
        let mut output = plan.execute(&[root_id], context.clone()).unwrap().remove(0);
        let mut rows = vec![];
        while let Some(batch) = output.next().await {
            rows.extend_from_slice(batch.unwrap().rows());
        }
        rows
    });
    assert_eq!(context.retained_bytes(), 0);
    result
}
