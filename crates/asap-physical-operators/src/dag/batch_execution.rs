//! Execute a bounded in-memory batch through native operators. This is also the
//! bridge for deployments whose boundary values are not yet streaming batches.
use super::{operators::Operator, values::Batch, Error, PhysicalDag, RunContext, SharedValue};
use futures::{FutureExt, StreamExt};

/// Every input is already in memory; the chain contains native operators only.
/// This deliberately does not enter a nested executor when called from a DAG
/// adapter. I/O belongs to source operators in the surrounding execution.
pub fn evaluate_batch(
    input: Batch,
    operators: Vec<Operator>,
    context: RunContext,
) -> Result<Vec<SharedValue<Batch>>, Error> {
    let mut graph = PhysicalDag::default();
    graph.add(
        0,
        vec![],
        Operator::source(input.schema().clone(), vec![input])?,
    )?;
    let mut root = 0;
    for operator in operators {
        graph.add(root + 1, vec![root], operator)?;
        root += 1;
    }
    evaluate_graph(graph, root, context)
}

/// Bind the ordered in-memory inputs of a native multi-input operator.
pub fn evaluate_inputs(
    inputs: Vec<Batch>,
    operator: Operator,
    context: RunContext,
) -> Result<Vec<SharedValue<Batch>>, Error> {
    let mut graph = PhysicalDag::default();
    let root = inputs.len() as u64;
    for (id, input) in inputs.into_iter().enumerate() {
        graph.add(
            id as u64,
            vec![],
            Operator::source(input.schema().clone(), vec![input])?,
        )?;
    }
    graph.add(root, (0..root).collect(), operator)?;
    evaluate_graph(graph, root, context)
}

/// Evaluate a native in-memory source, including scalar sources, in the caller's scope.
pub fn evaluate_source(
    source: Operator,
    context: RunContext,
) -> Result<Vec<SharedValue<Batch>>, Error> {
    let mut graph = PhysicalDag::default();
    graph.add(0, vec![], source)?;
    evaluate_graph(graph, 0, context)
}

fn evaluate_graph(
    graph: PhysicalDag<'_, Batch, super::values::Schema>,
    root: super::NodeId,
    context: RunContext,
) -> Result<Vec<SharedValue<Batch>>, Error> {
    let mut output = graph.execute(&[root], context)?.remove(0);
    let mut batches = Vec::new();
    loop {
        match output.next().now_or_never() {
            Some(Some(Ok(batch))) => batches.push(batch),
            Some(Some(Err(error))) => return Err(error),
            Some(None) => return Ok(batches),
            // Native operators have no I/O sources here. Pending is the
            // shared runtime's cooperative yield after a batch quantum.
            None => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::{operators::Expression, values::Value, Limits, Scope};
    use planner_types::{
        post_asap::{SummaryFamilyType, SummaryField, SummarySchema},
        pre_asap::DataType,
    };
    use std::sync::Arc;

    // Engine adapters can run the identical native chain from an outer executor.
    #[test]
    fn same_native_chain_inside_query_and_ingestion_execution() {
        let schema = Arc::new(SummarySchema {
            fields: vec![SummaryField {
                name: "value".into(),
                dtype: SummaryFamilyType::Plain(DataType::Float64),
                nullable: false,
            }],
            time_index: None,
        });
        for scope in [
            Scope::Query {
                evaluation_time_ms: 20,
                revision: 1,
            },
            Scope::Ingestion {
                window_start_ms: 10,
                window_end_ms: 20,
                revision: 1,
            },
        ] {
            let batch = Batch::try_new(schema.clone(), vec![vec![Value::Float64(7.)]]).unwrap();
            let negate = Operator::project(
                schema.clone(),
                vec![(
                    "value".into(),
                    Expression::Negate(Box::new(Expression::Column(0))),
                )],
            )
            .unwrap();
            let context = RunContext::new(scope, Limits::default()).unwrap();
            let result = futures::executor::block_on(async {
                evaluate_batch(batch, vec![negate], context.clone())
            })
            .unwrap();
            assert!(matches!(result[0].rows()[0][0], Value::Float64(-7.)));
            let source = Operator::scalar(Value::Float64(9.), DataType::Float64).unwrap();
            let scalar = evaluate_source(source, context).unwrap();
            assert!(matches!(scalar[0].rows()[0][0], Value::Float64(9.)));
        }
    }

    // Native sources may cross the runtime's cooperative batch quantum.
    #[test]
    fn in_memory_source_drives_cooperative_yields() {
        let schema = Arc::new(SummarySchema {
            fields: vec![],
            time_index: None,
        });
        let batch = Batch::try_new(schema.clone(), vec![vec![]]).unwrap();
        let source = Operator::source(schema, vec![batch; 65]).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits::default(),
        )
        .unwrap();
        assert_eq!(evaluate_source(source, context).unwrap().len(), 65);
    }

    // An adapter-held output must retain its parent's reservation after execution.
    #[test]
    fn returned_batches_keep_their_resource_reservation() {
        let schema = Arc::new(SummarySchema {
            fields: vec![],
            time_index: None,
        });
        let batch = Batch::try_new(schema.clone(), vec![vec![]]).unwrap();
        let bytes = batch.bytes();
        let source = Operator::source(schema, vec![batch]).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits {
                max_bytes: bytes,
                max_buffered_batches: 1,
            },
        )
        .unwrap();
        let held = evaluate_source(source.clone(), context.clone()).unwrap();
        assert_eq!(context.retained_bytes(), bytes);
        assert!(evaluate_source(source.clone(), context.clone()).is_err());
        drop(held);
        assert_eq!(context.retained_bytes(), 0);
        assert!(evaluate_source(source, context).is_ok());
    }

    // A cancelled surrounding execution also prevents its native computation.
    #[test]
    fn cancellation_is_not_bypassed_by_in_memory_execution() {
        let schema = Arc::new(SummarySchema {
            fields: vec![],
            time_index: None,
        });
        let batch = Batch::try_new(schema, vec![vec![]]).unwrap();
        let context = RunContext::new(
            Scope::Query {
                evaluation_time_ms: 0,
                revision: 0,
            },
            Limits::default(),
        )
        .unwrap();
        context.cancel();
        assert!(matches!(
            evaluate_batch(batch, vec![], context),
            Err(Error::Cancelled)
        ));
    }
}
