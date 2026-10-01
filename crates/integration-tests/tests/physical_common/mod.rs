use asap_physical_operators::{
    operators::Operator,
    physical_planner::{CompiledPhysicalDag, Source},
    runtime::{Limits, RunContext, Scope},
    values::Batch,
};
use futures::{executor::block_on, StreamExt};
use std::collections::BTreeMap;

pub fn execute(
    plan: &CompiledPhysicalDag,
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
