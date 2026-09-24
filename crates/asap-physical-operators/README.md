# ASAP physical operators

An independent Rust physical operator DAG runtime shared by ingestion time and
query time execution. The library requires neither backend engine, a server,
a storage implementation, Arrow nor DataFusion. DataFusion informed the design;
it is not the execution framework.

`plan::PhysicalDag` binds typed operator inputs to node IDs. Each execution starts
one producer per reachable node, shares output batches among its consumers, and
bounds buffering. Dropping one consumer does not cancel other consumers. A
`RunContext` carries query or ingestion scope, cancellation and byte accounting.
Executions use the caller's worker and worker-local streams, with no internal
thread pool. Poll multiple root streams concurrently when they share inputs.

`operators::Operator` implements native batch sources, scalar values,
projection, filtering, grouped exact aggregation, semi-join, grouped Sort and
Limit, vector-to-scalar conversion, Union, and summary construction/merge/readout.
Sort followed by Limit implements grouped ranking; no dedicated TopK physical
operator is needed. Summary construction updates state batch by batch. End of
input means the supplied query range or ingestion window is complete.

```rust
use asap_physical_operators::{
    expressions::Expression,
    operators::Operator,
    values::Value,
    plan::PhysicalDag,
    runtime::{Limits, RunContext, Scope},
};
use asap_physical_operators::planner::pre_asap::DataType;
use futures::{executor::block_on, StreamExt};

let source = Operator::scalar(Value::Int64(7), DataType::Int64)?;
let negate = Operator::project(source.schema(), vec![
    ("value".into(), Expression::Negate(Box::new(Expression::Column(0)))),
])?;
let mut plan = PhysicalDag::default();
plan.add(0, vec![], source)?;
plan.add(1, vec![0], negate)?;
let run = RunContext::new(
    Scope::Query { evaluation_time_ms: 1000, revision: 1 },
    Limits::default(),
)?;
let mut output = plan.execute(&[1], run)?.remove(0);
let batch = block_on(output.next()).unwrap()?;
assert!(matches!(batch.rows()[0][0], Value::Int64(-7)));
# Ok::<(), asap_physical_operators::dag::Error>(())
```

`binding::bind` accepts a post-ASAP DAG and explicit source bindings for
installed ingestion/storage frontiers. It rejects unsupported operations and
schema mismatches before starting a source. Implement `PhysicalOperator` for a
deployment source, including asynchronous I/O; computation operators remain in
the library. The public `planner` export identifies the exact Planner types used
by the crate. The native binder currently supports a subset of those types and
operations; it does not interpret an unknown node as external fallback.

Plain values preserve Planner scalar/collection types and nullability. Numeric
arithmetic uses matching Int64 or Float64 inputs; integer overflow is an error.
Boolean predicates use three-valued logic. Native summary states currently cover
exact Sum/Count/Min/Max/Rate/Increase, KLL, DDSketch, HLL and Float64 weighted CMS with a candidate heap. Binding checks family,
parameters and readout compatibility; source batches also validate state payloads.
Existing accumulator algorithms are reused as kernels behind these operators.

This crate is owned by ASAPPlanner. Its `planner-types` dependency is the local
IR crate, so a contract change and its execution tests belong in the same PR.
Deployments supply storage/ingestion sources and adapt output protocols. The
library has no ASAPQuery-backend dependency. Backend raw Scan remains a separate
deployment capability.

See [the design](../../docs/design_docs/physical-operators.md).

## Module boundaries

- `plan`: immutable graph, operator interface, schemas and execution properties.
- `runtime`: per-run streams, shared producers, memory reservations and cancellation.
- `expressions`: scalar evaluation; typed builders and the Planner expression adapter.
- `operators`: projection, filter, joins, aggregate/window, sort, limit and summary implementations.
- `sources`: raw-source interface, Scan and the memory connector.
- `binding`: Planner executable DAG binding and installed source frontiers.
- `summary_operators`: mathematical summary kernels, update adapters and accumulator traits.
- `stored_state`: persisted-state decoding, delta reconstruction and readout.
- `capability`: explicit kernel and native-batch/readout validation.

The old `dag`, `accumulators`, `factory`, `traits` and `arithmetic` paths remain re-exports for deployment
source compatibility. They contain no alternative execution implementations.

A source must declare `Boundedness::Bounded` to feed a blocking operator.
The default for a custom raw source is `Unknown`; query or ingestion scope alone
does not promise that its cursor ends. `PhysicalDag::properties` validates these
requirements before any source starts and returns boundedness and emission mode
for every reachable node. The memory connector declares finite input. Custom
physical sources expose the same facts through `PhysicalOperator::properties`.

Blocking operators reserve estimated workspace and yield cooperatively during
row processing and sort merges. Cancellation releases reservations when the
stream is polled or dropped. Individual scalar evaluations, bounded sort chunks
and sketch kernel calls are synchronous; this is not preemptive execution.
There is no spill or partitioned parallel execution in this implementation.
