use asap_executor::{
    physical_planner::CompiledPhysicalDAG,
    runtime::{Limits, RunContext, Scope},
    sources::{BindingDataSources, MemorySource, TableProvider},
    values::Batch,
};
use futures::{executor::block_on, StreamExt};
use std::collections::BTreeMap;
use std::sync::Arc;

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
                Arc::new(MemorySource::new(batch.schema().clone(), vec![batch]).unwrap())
                    as Arc<dyn TableProvider>,
            )
        })
        .collect();
    let data_sources = BindingDataSources {
        views: sources,
        ..Default::default()
    };
    let dag = plan.instantiate(&data_sources).unwrap();
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
/// where the DAG allows it, the placement precompute compilation requires.
#[allow(dead_code)] // Not every test binary sharing this module compiles precompute.
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
    use asap_executor::{physical_planner::compile, sources::DataSources};
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
    let mut tables = DataSources::default();
    tables
        .register(
            source,
            Arc::new(MemorySource::new(input, vec![batch]).unwrap()),
        )
        .unwrap();
    let data_sources = BindingDataSources {
        tables,
        ..Default::default()
    };
    let root_id = wire.roots[0] as u64;
    let plan = compile(&wire, &Default::default(), &[root_id])
        .unwrap()
        .instantiate(&data_sources)
        .unwrap();
    let context = RunContext::new(
        Scope::Query {
            evaluation_time_ms,
            revision: 1,
        },
        Limits::default(),
    )
    .unwrap();
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
