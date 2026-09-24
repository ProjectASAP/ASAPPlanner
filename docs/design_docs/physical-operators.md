# Shared physical operators and DAG execution

## Decision and ownership

ASAP implements and maintains its own physical operators, summary kernels and
DAG runtime. DataFusion is a reference for module organization and execution
contracts; #462 does not adopt its execution backend or a hybrid runtime.
`SummaryExpr` and the shared-producer DAG model remain unchanged.

`asap-physical-operators` lives beside Planner IR and lowering, depends on local
IR types, and has no ASAPQuery-backend dependency. Precompute and query engines
use the same computation: for example, Scan → KLL build/merge → quantile readout.
Deployments supply sources, evaluation windows, storage and publication.

| Responsibility | Owner |
| --- | --- |
| IR, schemas and parameters | `asap-types` |
| Lowering and candidate correctness | `asap-aware-mapping` |
| Operators, shared execution and source interface | `asap-physical-operators` |
| Summary encoding | `asap_sketch_codec` |
| External connectors, durable storage and serving | Deployment repositories |

## Module organization

```text
src/
  plan/              PhysicalDag, PhysicalOperator, properties, validation
  runtime/           streams, shared producers, context, memory, cancellation
  expressions/       scalar evaluation and Planner expression adaptation
  operators/
    projection.rs
    filter.rs
    joins/
    aggregate/       ordinary and temporal reductions
    sort.rs
    limit.rs
    summary/         build, merge and readout
    source.rs        literals, batch sources, union, scalar conversion
  sources/           raw-source API, Scan and memory connector
  binding/           Planner executable DAG → physical operators
  summary_operators/ mathematical kernels, factory and traits
  stored_state/      decoding, delta application, persisted-state readout
  capability.rs      support checks
  values.rs          typed rows and state validation
```

`plan` owns static contracts; `runtime` owns per-run state; operators own
construction checks and computation. `Expression` and `CompiledExpression`
share scalar execution under `expressions`. Summary kernels do not own scheduling
or storage selection. Kernel modules use operation names without `_accumulator`;
those old module names have no aliases. Top-level `dag`, `accumulators`,
`factory`, `traits` and `arithmetic` remain compatibility re-exports.

## Execution contract

- Validate topology, arity, schemas, supported bindings and input boundedness
  before starting sources. Unsupported operations fail without external fallback.
- Execute each reachable producer once per run. Consumers have independent
  cursors over shared outputs; separate runs never share mutable execution state.
- Run worker-local streams on the caller's worker. Deployments must poll consumers
  concurrently: bounded queues provide backpressure, and dropping one consumer
  leaves the others active.
- Propagate errors and cancellation. Long row loops and sort merge steps yield
  cooperatively; individual scalar and kernel calls remain synchronous.
- Charge retained outputs and estimated operator workspace against the byte
  budget. This is not an RSS or allocator-exact peak limit; source-owned data and
  temporary allocation peaks are not fully covered. Blocking operators do not spill.

`PhysicalOperator::properties` reports boundedness and emission;
`PhysicalDag::properties` derives them before execution. Sort, aggregate, temporal
reductions, joins, summary build/merge and vector-to-scalar require bounded input
and finalize after input ends. Projection, filter, limit, union and readout emit
incrementally. Global Limit bounds output cardinality; grouped Limit inherits
input boundedness. Neither promises a time deadline.

## Sources and binding

Raw Scan uses a registry keyed by Planner source identity. Binding checks metadata
without opening readers; execution lazily opens one cursor per reachable Scan
per run. Scan evaluates leaf predicates with three-valued logic, retaining only
TRUE. Projection, time selection and aggregation remain explicit operators.
Reader failures and schema drift fail execution.

`RawSource::boundedness` defaults to Unknown. Connectors must explicitly declare
finite snapshots/windows and handle cancellation and I/O buffering. Only an
immutable memory connector is included; external readers are deployment work.

The binder recognizes raw Scan inside the current `Fallback` leaf payload;
other retained expressions remain unsupported. Explicit source frontiers may
supply precomputed results. Stored-summary loading is separate from raw Scan:
deployments select compatible panes and provide coverage/revision scope.

## Supported computation

The library implements typed projection/filter, scalar arithmetic and booleans,
exact grouped aggregation, joins including semi-join, grouped Sort/Limit, Union,
scalar conversion, and summary build/merge/readout. Temporal reductions include
Rate, Increase, Sum, Avg, Min, Max, Count and histogram quantiles. Values preserve
Planner types/nullability and checked-division semantics; Count returns Int64.

Native summary edges support exact Sum/Count/Min/Max/Rate/Increase, KLL,
DDSketch, HLL, and Float64 weighted CMS/CountSketch with candidate heaps.
Weighted TopK consumes finalized per-series rates into a summary per group,
reads typed candidate identities/scores, then applies grouped Sort → Limit.
CMS uses nonnegative weights and minimum-row estimates; CountSketch accepts
signed weights and uses median sign-corrected estimates at positive odd depth.
Neither uses integer-count encoding or fixed-point updates.

Heap capacity differs from output k; ranking and merging do not prove candidate
completeness. Binding checks representation compatibility, not deployment accuracy
admission. Deployments provide a complete evaluation window or equivalent snapshot.

| Capability | Validation entry point |
| --- | --- |
| Update kernel and parameters | `capability::validate_summary_kernel` |
| Native state representation | `capability::validate_native_family` |
| Scalar readout | `capability::validate_native_readout` |
| Keyed readout and identity/score schema | `Operator::keyed_readout` |
| Complete executable plan | `binding::bind` / `bind_with_data_sources` |
| Persisted formats and reconstruction | `stored_state` |

Kernel or stored-state support alone does not imply executable-plan support.

## Scope and acceptance

Partitioned parallelism, disk spill, cost-based algorithm selection, physical
ordering/distribution properties and per-operator Explain/Analyze remain future
work. ASAP owns implementing and maintaining these capabilities when needed.
See the [DataFusion comparison](datafusion-execution-comparison.md) for tradeoffs.

Tests cover shared-producer diamonds, slow/dropped consumers, cancellation,
errors, resource accounting and run isolation; operator coverage includes types,
nulls, grouping, state compatibility and rejected bindings. The same summary
pipeline runs in ingestion and query scopes. Deployment acceptance additionally
requires real source binding, window/revision handling, publication and output
adaptation; external query forwarding does not demonstrate local execution.
