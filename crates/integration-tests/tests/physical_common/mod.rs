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
pub fn compile_post_asap_dag(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
) -> Result<asap_types::ir::export::PostAsapDAG, Box<dyn std::error::Error>> {
    let root = asap_types::ir::apply_lifecycle_timings(
        root,
        &Default::default(),
        &mut Default::default(),
    )?;
    Ok(asap_types::ir::export::compile_post_asap_dag(&root)?)
}

// Execute a selected relational DAG against real raw connectors, including scan predicates.
#[allow(dead_code)]
pub fn execute_raw_rows(
    root: &std::rc::Rc<asap_types::ir::OperatorNode>,
    rows: Vec<Vec<asap_physical_operators::values::Value>>,
) -> Vec<Vec<asap_physical_operators::values::Value>> {
    use asap_physical_operators::{
        physical_planner::bind_with_data_sources,
        runtime::{Limits, RunContext},
        sources::{DataSources, MemorySource},
    };
    use asap_types::ir::export::{NonASAPOpKind, PostAsapOperatorPayload};
    use futures::{executor::block_on, StreamExt};
    use std::sync::Arc;
    let wire = compile_post_asap_dag(root).unwrap();
    let scan = wire
        .nodes
        .iter()
        .find(|node| {
            matches!(
                node.payload,
                PostAsapOperatorPayload::Relational {
                    operator: NonASAPOpKind::Scan { .. }
                }
            )
        })
        .unwrap();
    let PostAsapOperatorPayload::Relational {
        operator: NonASAPOpKind::Scan { source, .. },
    } = &scan.payload
    else {
        unreachable!();
    };
    let input = Arc::new(scan.output_schema.clone());
    let batch = Batch::try_new(input.clone(), rows).unwrap();
    let mut sources = DataSources::default();
    sources
        .register(
            source.clone(),
            Arc::new(MemorySource::new(input, vec![batch]).unwrap()),
        )
        .unwrap();
    let root_id = u64::from(wire.root.0);
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
