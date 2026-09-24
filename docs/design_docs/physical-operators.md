# Shared Physical Operators and DAG Execution

## 1. Problem

ASAPPlanner produces logical Post-ASAP candidates, but downstream systems also
need concrete implementations to execute a selected computation. Before this PR,
there was no shared physical-operator library for precompute and query deployments
to reuse. Reimplementing execution in each deployment duplicates work and makes
consistency between Planner semantics and runtime behavior harder to maintain.

This design introduces `asap-physical-operators`: a shared library containing
**physical operators and a DAG runtime**. asap-fusion and ASAPQuery can use it to
bind selected computations and execute them with deployment-provided inputs.
The planning library remains deployment-independent; downstream systems retain
physical feasibility checks, final commitment and engine orchestration.

## 2. Goals and Non-goals

Goals:

- Share relational operators and summary build, merge and readout implementations.
- Execute shared dependencies once per run, with independent consumers.
- Share binding, cancellation and resource control across deployments.
- Test consistency between selected logical Post-ASAP DAGs and physical execution.

Non-goals for this PR are a complete precompute/query engine, external storage
connectors, partitioned parallelism, sharding, disk spill and cost-based physical
algorithm selection. The library provides a common place for future execution
optimizations; it does not implement those optimizations here.

## 3. Architecture

### 3.1 End-to-end flow

Terminology follows the [ASAPPlanner design overview](architecture/README.md).
The planning library determines logical candidates. Downstream selects a
computation and commits to a feasible physical realization. The shared binder
constructs supported implementations, and the shared runtime executes them.
The deployment determines when, where and with which inputs execution happens.

```text
SQL / PromQL + planning workload and applicable evidence
    ↓ frontend lowering
Canonical Pre-ASAP QueryExpr roots
    ↓ logical candidate search
PlanSpace: logical Post-ASAP candidate DAG space
    ↓ selection and assembly
Selected logical Post-ASAP DAG: SummaryNode / SummaryExpr
    ↓ compile_executable_dag(...)
ExecutableDag: structural representation compiled from the selected DAG
    ↓ physical binding + deployment inputs and execution roots
PhysicalDag: concrete operators and their dependencies
    ↓ execute(..., RunContext)
Results
```

This shows the selected-DAG path used by this library. `PlanSpace` is Planner's
primary output; selection and assembly are subsequent steps, not an implicit
physical deployment decision. Downstream may consume candidates directly or use
Planner's optional selection helpers. Lifecycle-aware helpers additionally
produce a `SummaryMaintenanceLifecyclePlan`; this runtime does not choose or
schedule that lifecycle. See the [planning workflows](architecture/input-output-workflow.md).

### 3.2 Representation boundaries

| Architecture term | Rust representation | Meaning |
| --- | --- | --- |
| Pre-ASAP IR | `QueryExpr` | Original exact query semantics and output shape. |
| Logical candidate DAG space | `PlanSpace` | Alternatives and their logical guarantees/rejection reasons, before physical commitment. |
| Selected logical Post-ASAP DAG | `SummaryNode` / `SummaryExpr` | Selected logical summary semantics, including families, parameters, composition and readout. |
| Compiled representation of the selected Post-ASAP DAG | `ExecutableDag` | Node identities, schemas, execution assignments and shared dependencies exported for binding. Contains no instantiated physical operators. |
| Physical DAG used by this runtime | `asap_physical_operators::plan::PhysicalDag` | Concrete operator implementations connected for execution. |

`ExecutableDag` is derived from the selected logical Post-ASAP DAG; it is not a
replacement name for Post-ASAP IR. A `SummaryNode` describes summary computation
but does not consume batches, update mutable sketches or coordinate cancellation.
Sharing its identity expresses a dependency; runtime coordination realizes shared
execution.

The architecture's [physical-plan integration](architecture/physical-plan-integration.md)
also describes physical alternatives and analytical resource estimation. The
runtime `PhysicalDag` here should not be confused with a cost-model representation:
this PR does not automatically connect binding to analytical costing or search
across physical alternatives.

### 3.3 Component responsibilities

**The shared runtime executes one DAG run. Deployment engines decide which work
to run, when to run it and how to use its results.**

| Component | Responsibility |
| --- | --- |
| ASAPPlanner planning library | Generate logical Post-ASAP candidates; optionally select and assemble logical DAGs. |
| `compile_executable_dag` in `asap-types` | Export a selected logical Post-ASAP DAG as an `ExecutableDag`, preserving shared identity. |
| Binder | Construct supported physical implementations and validate their connections. |
| Physical operators | Perform relational and summary computation. |
| Shared runtime | Coordinate dependencies, shared producers and per-run resources. |
| asap-fusion / ASAPQuery | Decide physical feasibility and deployment; schedule runs, select storage/windows/revisions, persist/publish and serve results. |

Keeping implementations in the ASAPPlanner repository does not move deployment
commitment into the planning library. The deployments reuse this crate as part
of their physical implementation rather than duplicating its DAG execution.

## 4. Execution Model

### 4.1 Binding

The deployment supplies an `ExecutableDag`, execution roots and inputs. The
[binder](../../crates/asap-physical-operators/src/binding/mod.rs) traverses reachable
dependencies and constructs supported operators. A supplied intermediate result
cuts traversal at that node, enabling sub-DAG execution. Section 6 defines this
input boundary. Unsupported computation fails binding.

### 4.2 DAG execution

[`PhysicalDag::execute`](../../crates/asap-physical-operators/src/plan/mod.rs)
creates per-run execution state using a `RunContext` and returns output streams.
The deployment drives these streams and handles their results. Execution is
worker-local; the library does not supply a deployment scheduler or thread pool.
Separate runs reuse the plan structure while keeping mutable execution state
independent.

### 4.3 Shared producers

A reachable producer executes once per run, even when several consumers depend
on it. Each consumer has its own cursor over the shared output. Bounded queues
apply backpressure, so deployments must poll multiple requested output streams
concurrently. Dropping one consumer leaves other consumers active.

### 4.4 Example: summary build → merge → readout

```text
Precompute deployment                 Query deployment

raw data                              compatible stored KLL states
    ↓                                             ↓
KLL build                                      KLL merge
    ↓                                         ┌───┴───┐
stored KLL state                               ↓       ↓
(deployment persists)                    p50 readout  p99 readout
```

The precompute engine provides a finite input window and persists the returned
state. The query engine selects compatible stored states and supplies them at
an input frontier. Binding includes only the required downstream computation.
The merge executes once, and both readouts independently consume its output.

Both engines reuse the same build, merge and readout implementations. This is
the E2E change introduced by the PR: deployments gain a shared binding and
execution path for selected computations, while retaining storage and scheduling.

## 5. Execution Contracts

| Contract | Invariant |
| --- | --- |
| C1 — Validate before execution | Topology, schemas, arity, supported operations and required boundedness are checked before sources start. Runtime data and reader failures remain execution errors. |
| C2 — Execute each producer once per run | Multiple consumers share one producer execution. |
| C3 — Isolate runs | Independent executions do not share mutable operator execution state. |
| C4 — Apply backpressure | Producer/consumer communication uses bounded queues and independent cursors. |
| C5 — Propagate failure and cancellation | Errors are not empty results; long computation loops cooperate with cancellation. |
| C6 — Enforce the tracked resource budget | Retained outputs and estimated operator workspace count against the run's byte budget. |

Blocking operators, including sort, aggregation, joins and summary build/merge,
require bounded input and finalize after input ends. Unknown source boundedness
is insufficient. Projection, filter and readout can emit incrementally.

Cancellation is cooperative: individual scalar and sketch-kernel calls remain
synchronous. Resource accounting is not an allocator-exact or process-RSS limit;
source-owned data and temporary allocation peaks are not fully covered. There
is no spill path, so exceeding the tracked byte budget fails execution.

## 6. Inputs and Execution Frontiers

A deployment can provide raw data at a Scan or already-computed results at an
intermediate node. These are two ways to supply the inputs of one execution.

### 6.1 Raw data sources

`bind_with_data_sources` resolves supported raw Scan leaves through a registry
keyed by Planner source identity. Binding checks metadata; execution lazily opens
readers. Connectors must declare finite snapshots/windows when required and
handle cancellation and I/O buffering. Reader failures and schema drift fail
execution. Only a memory connector is included in this PR.

The current binder recognizes raw Scan inside a retained Pre-ASAP leaf payload.
Other unsupported retained expressions fail binding; arbitrary Pre-ASAP execution
is not implied by support for Scan.

### 6.2 Supplied intermediate results

```text
Scan → KLL build → KLL merge → quantile readout
           ↑
    deployment may supply this node's output from stored state
```

When a deployment supplies a compatible result, binding treats that node as an
execution frontier and does not bind its upstream dependencies. The supplied
operator must match the node's declared schema. Deployments therefore need not
manually construct physical DAGs to reuse precomputed work.

Schema compatibility does not establish window coverage, revision correctness,
maintenance readiness or accuracy guarantees. Planning and deployment must
establish these before committing to execution. Stored-state decoding provides
format support, not a policy for selecting valid stored panes.

## 7. Implementation Organization

| Module | Owns |
| --- | --- |
| `plan` | Physical DAG/operator contracts, properties and validation |
| `binding` | Compiled Post-ASAP representation → concrete operators |
| `runtime` | Per-run execution, streams, shared producers and resource control |
| `operators` | Batch implementations of relational and temporal computation |
| `expressions` | Scalar evaluation and Planner expression adaptation |
| `operators/summary` | Physical summary build, merge and readout |
| `summary_kernels` | Planner-facing sketch adapters and exact accumulators |
| `sources` | Input interfaces, Scan and memory connector |
| `stored_state` | Persisted summary decoding, delta application and reconstruction |

Sketch algorithms themselves, including weighted CMS/CountSketch, belong to
`asap_sketchlib`. Kernel availability does not imply support for native binding,
every readout or every stored-state format; these capabilities are checked
separately.

Locating the library alongside Planner lets one PR change an IR node and its
physical implementation. It also enables tests across internal planning and
execution APIs without coordinating repositories. Deployment policies and
external connectors remain outside the library.

## 8. Alternatives Considered

### 8.1 ASAP-owned runtime — selected

ASAP owns physical operators and DAG execution, following DataFusion's separation
of contracts, runtime and concrete implementations. This provides direct control
over native summary-state edges and shared producers, common computation across
deployments, and tests spanning logical planning and execution.

The cost is ownership of operator correctness, resource management and future
parallelism, spill and physical optimization. There is no measured claim that
this runtime has lower overhead than DataFusion.

### 8.2 DataFusion extension

DataFusion offers established physical implementations and execution machinery.
Reusing it would require integrating ASAP's logical summary semantics, state
transport, sharing, partitioning and lifecycle contracts. Extensions do not
inherently require changes to DataFusion core, but existing optimizations are
usable only when those contracts preserve ASAP semantics.

This PR chooses native execution; a DataFusion backend or hybrid runtime is
outside its scope. The [DataFusion comparison](datafusion-execution-comparison.md)
provides the detailed tradeoffs.

## 9. Validation

Acceptance has three levels:

| Level | Required behavior | Coverage today |
| --- | --- | --- |
| Operator correctness | Correct results and schemas for supported types, edge cases and errors; resource contracts hold. | Unit tests, [semantic tests](../../crates/asap-physical-operators/tests/physical_semantics.rs) and [resource tests](../../crates/asap-physical-operators/tests/blocking_resources.rs). |
| Physical DAG correctness | Correct composition, dependencies, shared producers, cancellation and independent runs. | Runtime unit tests and [DAG integration tests](../../crates/asap-physical-operators/tests/physical_dag.rs). |
| Planner → execution correctness | Selected logical Post-ASAP DAGs compile, bind and execute with the intended semantics. | [Weighted TopK integration](../../crates/asap-physical-operators/tests/weighted_topk_binding.rs) starts from PromQL, selects a candidate and executes with supplied rate results. |

The third level is partial: weighted TopK injects finalized rates at a frontier,
so it does not execute raw time-series input through rate calculation.
[Raw Scan integration](../../crates/asap-physical-operators/tests/raw_scan.rs)
covers Scan → Sort → Limit using a manually constructed `ExecutableDag`.

Still required are a complete SQL/PromQL → raw input → planning → binding → native
results test and broader combinations of Planner-generated operators. Deployment
acceptance additionally needs real connectors, window/revision selection,
persistence, publication and serving.

Existing automated tests were run locally. No separate manual deployment-level
verification was performed. The documentation update does not add test coverage.

## 10. Limitations and Future Work

Execution currently targets supported operations over bounded inputs where
blocking computation is required. It is not complete SQL/PromQL execution, and
successful logical candidate construction does not prove physical feasibility.
Downstream must reject candidates without a complete supported realization for
the chosen execution boundary.

Partitioned parallelism, sharding, spill, physical algorithm selection and richer
ordering/distribution properties require further design and implementation.
Connecting runtime implementations to analytical physical costing is also a
separate integration task. These extensions must preserve C1–C6 and the planning
and deployment ownership boundaries above.
