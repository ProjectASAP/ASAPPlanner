# Shared physical operators and DAG execution

## Decision

ASAP owns an independent physical operator library and DAG runtime. Precompute
and query engines bind inputs and consume outputs from the same library. The
operator defines computation; the engine supplies ingestion time or query time,
window boundaries, storage access and publication. There is no second execution
algorithm selected by phase.

The library lives in the ASAPPlanner workspace as `asap-physical-operators`,
alongside `asap-types` and logical-to-physical lowering. It depends on those local
IR types and never on ASAPQuery-backend. New IR nodes and their implementations
can be reviewed and tested in one Planner PR. Private implementation tests live
inside their owning crate; integration tests exercise exported physical DAGs.
Its native operators can execute independently of either backend engine.
Engine integration must use these operators for computation, rather than merely
using the shared scheduler around a second implementation.

## Workspace organization

[DataFusion's physical-plan crate](https://github.com/apache/datafusion/tree/main/datafusion/physical-plan)
is the ownership reference: it owns the execution-plan interface, concrete
operators, streams, metrics and operator tests within the same repository as
planning. ASAP follows that repository boundary, while retaining its own DAG
execution model.

| Responsibility | ASAP owner |
| --- | --- |
| post-ASAP nodes, schemas, parameters and execution phase | `asap-types` |
| Logical-to-physical lowering and candidate correctness | `asap-aware-mapping` |
| Physical operator implementations, input/output validation and streams | `asap-physical-operators` |
| Shared-producer scheduling, cancellation and resource accounting | `asap-physical-operators` |
| Summary state encoding | `asap_sketch_codec` |
| Sources, durable stores, publication and serving protocols | Deployment repositories |

The IR crate does not depend on execution. The physical operator crate depends
on the local IR crate. Operator unit tests can exercise private implementation
details; Planner integration tests check that emitted DAGs bind and execute.
Runtime values preserve the IR schema instead of redefining its type semantics.

## Execution contract

An immutable plan describes typed nodes and dependency edges. Each execution
creates its own operator state. One producer may have multiple consumers; the
producer executes once in that run and sends the same outputs to all consumers.
Separate runs, query evaluation times and ingestion windows do not share mutable
state. Request-local caching of intermediate results is scoped to execution.

The runtime validates dependencies, schemas, arity and cycles before sources
start. Each consumer advances independently. Bounded queues apply backpressure;
dropping one consumer does not cancel other consumers. Whole-run cancellation
wakes readers and releases queued work as streams are polled or dropped.

Execution runs on the caller's worker without an internal thread pool. Active
streams are worker-local. Deployments poll all consumers concurrently. The byte
budget accounts for retained outputs and native operator state, including outputs
held after queue eviction. It is not an RSS limit: source-owned data, temporary
allocation peaks and allocator overhead remain outside that estimate. Blocking
operators currently have no spill implementation.

## Operator coverage

Native operations include scalar sources, typed Project and Filter, arithmetic
and boolean expressions, exact grouped aggregation, relational joins (including semi-join), grouped Sort and
Limit, Union, vector-to-scalar conversion, and summary construction, merge and
readout. Window operators consume Planner aggregate intents for Rate, Increase,
Sum, Avg, Min, Max, Count and histogram quantiles. Deployments supply window
boundaries and bound columns; the computation is identical in either phase.
Count outputs Int64. Binary expressions use Planner arithmetic/comparison kinds
and enforce its checked-division domains.

Grouped TopK composes Sort and Limit within each group; candidate
completeness is an earlier pruning obligation.

Values retain Planner types and nullability. Native summary batches currently
support exact Sum/Count/Min/Max/Rate/Increase, KLL, DDSketch and HLL. Stored-summary decoding, delta reconstruction, exact finalization and
family-specific SketchQuery readout also live in this library. Deployment code
selects compatible panes and supplies source batches. Stored-state kernels do
not imply native batch bindings for every family. Unsupported expressions,
state families and parameters must be rejected during binding, without an
implicit external fallback. The backend retains source, storage, publication and protocol adapters.
Computation must bind to Planner operations without a second backend operator
vocabulary. Binding rejects unsupported Planner nodes before starting sources.

Deployments provide explicit storage or ingestion source frontiers. A supplied
batch source is not a backend raw Scan implementation. Local backend raw Scan
is deferred; a raw-only library test does not establish that deployment capability.

## DataFusion reuse vs independent implementation

| Decision dimension | Reuse DataFusion | Independent ASAP implementation |
| --- | --- | --- |
| General computation | Reuse mature Arrow operators and expression execution | Implement and test the supported Planner vocabulary explicitly |
| Shared DAG producer | Shared plan references need an explicit execution-sharing and buffering policy | One producer and independent consumer cursors are part of the runtime contract |
| Summary lifecycle | Add custom summary state operators to the framework | Summary construction, merge and readout are native capabilities |
| In-memory representation | Operators exchange Arrow RecordBatch values. Custom summary state may not map naturally to Arrow and can require an explicit encoding, wrapper or conversion, with associated integration and potential copying costs | Native values can carry ASAP-defined summary state directly, without requiring every state format to fit Arrow; the library must still define and validate state types, ownership and compatibility |
| Engine reuse | Adapt both engines to DataFusion's execution model | Both engines bind the same ASAP interfaces |
| Engineering cost | Less generic operator work; integration and semantic adaptation remain | More operator, typing, scheduling and resource-accounting responsibility |

DataFusion is a design reference, not this library's execution dependency. This
choice does not claim that DataFusion cannot express shared dependencies. ASAP
chooses direct ownership of execution sharing and summary-state semantics across
both engines. Mathematical sketch kernels remain reusable implementation details.

## Acceptance

Independent tests must execute shared-producer diamonds without duplicated work
or deadlock, exercise slow and dropped consumers, propagate cancellation and
errors, retain memory accounting, and isolate separate executions. Operator tests
must cover types, nulls, grouped limits, state compatibility and unsupported
bindings. The same summary pipeline must run at ingestion time and query time.

Backend integration adds deployment acceptance for source binding, window and revision
scope, durable publication and query output adaptation. External exact forwarding
does not count as evidence that a local operator was implemented.
