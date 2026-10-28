# ASAP executor

An independent Rust physical operator DAG runtime shared by ingestion time and
query time execution. The library requires neither backend engine, a server,
a storage implementation nor DataFusion. Batches are Arrow columns
(`values::Batch`); DataFusion informed the design, it is not the execution
framework.

`plan::PhysicalDAG` binds typed operator inputs to node IDs. Each execution starts
one producer per reachable node, shares output batches among its consumers, and
bounds buffering. Dropping one consumer does not cancel other consumers. A
`RunContext` carries query or ingestion scope, cancellation and byte accounting.
Executions use the caller's worker and worker-local streams, with no internal
thread pool. Poll multiple root streams concurrently when they share inputs.

`operators::Operator` implements native batch sources, scalar values,
projection, filtering, grouped exact aggregation, semi-join, grouped Sort and
Limit, vector-to-scalar conversion, Union, and summary construction/merge/evaluation.
Sort followed by Limit implements grouped ranking; no dedicated TopK physical
operator is needed. Summary construction updates state batch by batch. End of
input means the supplied query range or ingestion window is complete.

```rust
use asap_executor::{
    expressions::Expression,
    operators::Operator,
    values::Value,
    plan::PhysicalDAG,
    runtime::{Limits, RunContext, Scope},
};
use asap_executor::planner::ir::schema::DataType;
use futures::{executor::block_on, StreamExt};

let source = Operator::scalar(Value::Int64(7), DataType::Int64)?;
let negate = Operator::project(source.schema(), vec![
    ("value".into(), Expression::Negate(Box::new(Expression::Column(0)))),
])?;
let mut plan = PhysicalDAG::default();
plan.add(0, vec![], source)?;
plan.add(1, vec![0], negate)?;
let run = RunContext::new(
    Scope::Query { evaluation_time_ms: 1000 },
    Limits::default(),
)?;
let mut output = plan.execute(&[1], run)?.remove(0);
let batch = block_on(output.next()).unwrap()?;
assert!(matches!(batch.to_rows()[0][0], Value::Int64(-7)));
# Ok::<(), asap_executor::Error>(())
```

`physical_planner::compile` accepts a Planner `PhysicalASAPDAG` and the materialized views it reads.
The result is instantiated with the deployment's data store. It rejects unsupported operations and
schema mismatches before starting a source. Implement `sources::DataStore`
for deployment data (raw data and materialized views), including
asynchronous I/O; computation operators remain in the library. The public `planner` export identifies the exact Planner types used
by the crate. The physical compiler currently supports a subset of those types and
operations; it does not interpret an unknown node as external fallback: a
node without a native lowering fails compilation, and the deployment runtime
must execute it.

Plain values preserve Planner scalar/collection types and nullability. Numeric
arithmetic uses matching Int64 or Float64 inputs; integer overflow is an error.
Boolean predicates use three-valued logic. Native summary states currently cover
exact Sum/Count/Min/Max/Rate/Increase, KLL, DDSketch, HLL and Float64 weighted CMS and CountSketch with candidate heaps. Binding checks family,
parameters and evaluation compatibility; source batches also validate state payloads.
Existing accumulator algorithms are reused as kernels behind these operators.

This crate is owned by ASAPPlanner. Its `planner-types` dependency is the local
IR crate, so a contract change and its execution tests belong in the same PR.
Deployments supply storage/ingestion sources and adapt output protocols. The
library has no ASAPQuery-backend dependency. Backend raw Scan remains a separate
deployment capability.

See [the design](../../docs/design_docs/physical-planning-and-deployment.md).

## Module boundaries

- `plan`: immutable DAG, operator interface, schemas and execution properties.
- `runtime`: per-run streams, shared producers, memory reservations and cancellation.
- `expressions`: scalar evaluation; typed builders and the Planner expression adapter.
- `operators`: projection, filter, joins, aggregate/window, sort, limit and summary implementations.
- `sources`: the `DataStore` data interface, `Scan` and the in-memory store.
- `physical_planner`: native operator lowering, typed input slots and checked instantiation.
- `summary_kernels`: in-memory summary state over `asap_sketchlib` and exact Planner state: merge, typed evaluation and update adapters.
- `evaluation`: evaluations over merged exact summary states.
- `capability`: explicit kernel, native-batch and typed evaluation validation.

The `dag`, `factory`, `traits` and `arithmetic` paths are re-exports. They contain no alternative
execution implementations.

Sketch algorithms and their state encodings belong to `asap_sketchlib`. Wire decoding, delta
frames, edge sampling and storage statistics belong to deployments. Kernels hold one population's
state; operators own grouping.

A source must be `Boundedness::Bounded` to feed a blocking operator. Every
read of a `DataStore` is bounded: a store returns the rows of one run's
request and ends. `PhysicalDAG::properties` validates these requirements
before any source starts and returns boundedness and emission mode for every
reachable node. Custom physical sources expose the same facts through
`PhysicalOperator::properties`.

Blocking operators reserve estimated workspace and yield cooperatively during
row processing and sort merges. Cancellation releases reservations when the
stream is polled or dropped. Individual scalar evaluations, bounded sort chunks
and sketch kernel calls are synchronous; this is not preemptive execution.
There is no spill or partitioned parallel execution in this implementation.

## Physical compilation and deployment inputs

`physical_planner::compile` accepts a Planner `PhysicalASAPDAG`, the
materialized views the computation reads (`PhysicalASAPDAG::materialized_views`)
and output roots. Every `Scan` leaf is a raw data slot and every given view a
materialized view slot; each has its node's `output_schema`. It returns a
reusable `CompiledPhysicalDAG` containing selected native operators and no
live readers. Compilation validates schemas, input ordering, sharing and
boundedness before deployment source access.

A deployment calls `CompiledPhysicalDAG::instantiate` with one `DataStore`
handle per run, which presents one consistent state to every read of that
run. Each raw data slot calls `read_raw_data(source, RawDataRequest)` (time
range, the slot's schema, predicates); the request is a hint, and the
executor re-applies its time range and predicates. Each materialized view
slot calls `read_materialized_view(view, MaterializedViewRequest)` (the time
range its state summarizes, and the slot's schema); the store returns exactly
those states, which the executor does not re-filter or charge to the run. A
time series is a table `(ts, value, *labels)` read like a SQL table: the DAG's
own `TimeRange` keeps each window, an instant selector's latest sample per
series, and drops Prometheus stale markers. The DAG executes through the
shared runtime with independent per-run state. Window coverage and
maintenance-policy admission remain deployment/planning contracts; this
compiler does not discover storage or silently change a selected maintenance
strategy.

Pane construction and geometry belong to deployment. A deployment refreshes
each pane view once per pane and returns the pane states from its store; the
query DAG merges and reads them out as computation.
