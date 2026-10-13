use asap_physical_operators::{
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
            asap_types::ir::Materialization::MatAtIngestion(asap_types::ir::Medium::Memory),
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
