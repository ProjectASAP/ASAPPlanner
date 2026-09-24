# Shared physical operators and DAG execution

## Problem and goals

ASAPPlanner selects computations and summaries, but deployments also need concrete
implementations that execute those plans. Before this PR, there was no shared
physical-operator library for precompute and query deployments to reuse. Leaving
execution to each deployment duplicates implementation work and makes consistency
between Planner IR and executed behavior harder to maintain.

This design adds **both a physical-operator library and a runtime for executing
its DAGs** in `asap-physical-operators`. The intended consumers are developers of
asap-fusion and ASAPQuery. They can bind Planner-generated plans to shared
operators and execute a DAG or sub-DAG using deployment-provided inputs.

The goals are to:

- Implement ordinary relational computation and summary build, merge and readout
  behind common execution contracts.
- Preserve shared dependencies: one producer executes once per run even when
  several consumers read its output.
- Share binding, cancellation and resource control across precompute and query
  deployments while leaving engine orchestration with those deployments.
- Test Planner output and physical execution together so that changes to IR,
  schemas and implementations stay consistent.

Partitioned parallelism, sharding, disk spill and cost-based physical algorithm
selection are future work. This PR provides a common place to implement them;
it does not deliver those optimizations or a complete deployment engine.

## From a selected plan to results

```text
SQL / PromQL
    ↓ frontend
QueryExpr
    ↓ ASAP planning and summary selection
SummaryNode containing SummaryExpr
    ↓ compile_executable_dag(...)
ExecutableDag
    ↓ binding::bind(...) + deployment-provided inputs and roots
PhysicalDag
    ↓ execute(..., RunContext)
Results
```

These representations serve different purposes:

| Representation | Responsibility |
| --- | --- |
| `SummaryNode` / `SummaryExpr` | Describe the selected computation, including summary families, parameters and execution timing. |
| `ExecutableDag` | Record node identities, schemas and shared dependencies for binding. Despite its name, it contains no running physical operators. |
| `PhysicalDag` | Connect concrete physical-operator implementations that the shared runtime can execute. |

A summary node already contains planning decisions; it is not just an unresolved
logical operator. It still does not implement batch consumption, mutable sketch
updates, output production or cancellation. Those belong to physical operators
and their runtime. Likewise, sharing a `SummaryNode` reference describes shared
computation; runtime coordination makes that sharing effective during execution.

[`compile_executable_dag`](../../crates/types/src/post_asap/executable_dag.rs)
compiles the selected root and preserves shared node identity. The
[binder](../../crates/asap-physical-operators/src/binding/mod.rs) walks dependencies
from the requested roots, constructs supported operators and checks their
schemas. The resulting
[`PhysicalDag`](../../crates/asap-physical-operators/src/plan/mod.rs) is the input
to execution. Deployments normally use this path; they need not manually assemble
physical DAGs as low-level tests do.

## Deployment boundary

The shared runtime executes an individual DAG run. A deployment engine decides
which work to run, when to run it and how to use its results.

| Shared library | asap-fusion / ASAPQuery deployment |
| --- | --- |
| Bind supported plan nodes and validate schemas | Select execution roots and approved input frontiers |
| Execute operators and coordinate shared producers | Schedule precompute/query runs and drive output streams |
| Track per-run state, cancellation and estimated memory | Set limits, evaluation windows and revision scope |
| Decode and combine supported stored-summary formats | Choose compatible stored panes and ensure coverage |
| Produce typed results | Persist/publish summaries or adapt and serve query results |

“Deployment sources” means either raw-data connectors registered by Planner
source identity, or operators supplying results at an explicit node frontier.
For example, a query engine can supply a stored KLL state at the point where an
ingestion run would have built it. Binding stops traversing upstream dependencies
at that frontier and requires the supplied operator to match the node's schema.
This lets deployments execute the relevant sub-DAG without rebuilding upstream
computation.

`bind_with_data_sources` resolves supported raw Scan leaves through the connector
registry. Readers open lazily during execution. Only a memory connector is
included here; external storage access belongs to deployments. Unsupported
retained expressions fail binding rather than silently forwarding execution to
another engine. A schema-compatible frontier alone does not establish window
coverage, revision correctness or summary accuracy; those remain deployment and
planning responsibilities.

### Example: one summary, multiple answers

An ingestion run can read a finite window, build a KLL summary and return it for
the precompute engine to persist. A query run can load compatible partial states,
merge them and feed the merged state to multiple quantile readouts. The merge
producer runs once, and each readout consumes its output independently.

The library supplies the same build, merge and readout implementations for both
uses. The deployment supplies storage selection, scheduling and publication.
This is the before/after effect: selected plans gain a shared execution path
instead of requiring every deployment to implement these computations itself.

## Execution contracts

The design separates reusable plan structure from mutable per-run state:

- Validate topology, arity, schemas, supported operations and required input
  boundedness before starting sources. Errors are not empty results.
- Execute each reachable producer once per run. Consumers have independent
  cursors; separate runs have independent mutable execution state.
- Use bounded queues for backpressure. Streams run on the caller's worker, and
  deployments must poll multiple requested outputs concurrently. Dropping one
  consumer leaves its siblings active.
- Propagate errors and cancellation, and yield cooperatively in long loops.
  Individual scalar and sketch-kernel calls remain synchronous.
- Account for retained outputs and estimated operator workspace against a byte
  budget. This is not an allocator-exact or process-RSS limit; source-owned data
  and temporary allocation peaks are not fully covered.

Blocking operators, including sort, aggregation, joins and summary build/merge,
require bounded input and finalize after input ends. Connectors must explicitly
declare finite snapshots or windows; unknown boundedness is insufficient.
Projection, filter and readout can emit incrementally. There is no spill path,
so workloads exceeding the tracked memory budget fail.

## Implementation ownership and alternatives

ASAP implements its own operators and DAG runtime, borrowing DataFusion's
separation of execution contracts and module responsibilities. This gives ASAP
direct ownership of summary-state edges and shared-producer execution, with the
cost of maintaining correctness, resource control and future optimizations.
It is not a claim of lower runtime overhead. DataFusion extension is a viable
alternative and does not inherently require modifying its core; the
[comparison](datafusion-execution-comparison.md) explains that tradeoff.

Within the crate, `plan` owns contracts and validation, `binding` constructs
operators, `runtime` owns per-run execution, `expressions` evaluates scalars,
`operators` implements batch computation, and `sources` defines input access.
`operators/summary` adapts summary computation to the batch/DAG interface.
`summary_kernels` contains Planner-facing sketch adapters and exact accumulators;
sketch algorithms, including weighted CMS/CountSketch, belong to `asap_sketchlib`.
`stored_state` handles supported persisted-state decoding and reconstruction.
Kernel availability does not by itself imply support for native binding or every
stored-state format.

Keeping the library in ASAPPlanner allows an IR change and its physical
implementation to be reviewed in one PR. It also permits tests across planning
and execution, including internal APIs, without coordinating changes across
separate repositories. Deployment connectors and engine policies remain outside
this library.

## Validation and remaining gaps

Acceptance has three levels. Each checks a different boundary:

| Level | Required behavior | Existing automated coverage |
| --- | --- | --- |
| Individual operators | Correct values and schemas for supported types, edge cases and errors; enforce resource contracts. | Operator unit tests, [semantic tests](../../crates/asap-physical-operators/tests/physical_semantics.rs), [resource tests](../../crates/asap-physical-operators/tests/blocking_resources.rs). |
| Physical DAG execution | Compose operators correctly; respect dependencies, shared producers, cancellation and independent runs. | Runtime unit tests and [DAG integration tests](../../crates/asap-physical-operators/tests/physical_dag.rs), including summary build/merge/readout and shared consumers. |
| Planner-to-execution integration | Bind actual Planner-generated DAGs and execute with the intended types and dependencies. | [Weighted TopK integration](../../crates/asap-physical-operators/tests/weighted_topk_binding.rs) starts from PromQL and executes the selected plan with supplied rate results. |

The third level has partial coverage: the weighted TopK test injects finalized
rates at an explicit frontier rather than reading raw time-series samples.
[Raw Scan integration](../../crates/asap-physical-operators/tests/raw_scan.rs)
executes Scan → Sort → Limit from a manually constructed `ExecutableDag`.
Neither establishes complete SQL/PromQL text → raw data → planning → native
results coverage. That full-path test remains a testing requirement, as does
broader coverage of Planner-generated operator combinations.

The automated tests were run locally. No separate manual deployment-level
verification was performed. Deployment acceptance must additionally verify real
connectors, window/revision selection, persistence, publication and result
serving; the library tests do not establish those behaviors.
