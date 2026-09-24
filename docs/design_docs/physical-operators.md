# Shared Physical Operators and DAG Execution

## 1. Problem

ASAPPlanner describes valid summary computations. Precompute and query engines
need concrete operators to execute them. Implementing these operators and their
execution separately duplicates work and makes it harder to preserve Planner
semantics across deployments.

This design provides a shared physical execution library for asap-fusion and
ASAPQuery. It contains both physical operators and the runtime for executing a
DAG. Deployment compilers use the shared physical operator vocabulary to describe
computation; deployment engines supply inputs and invoke the shared executor.

The intended outcome is that the same summary build, merge and readout can run
in either engine with the same semantics and execution contracts.

## 2. Goals and Non-goals

Goals:

- Share relational and summary operators across precompute and query execution.
- Preserve logical semantics when compiling a computation to physical operators.
- Execute shared producers once per run, with independent consumers.
- Provide common failure, cancellation, backpressure and resource contracts.
- Keep deployment decisions separate from computation execution.

This library does not choose deployment placement, manage durable storage,
schedule maintenance or serve requests. Partitioned parallelism, sharding, spill
and cost-based algorithm selection are outside the initial scope.

## 3. Architecture

### 3.1 Three layers

Terminology follows the [ASAPPlanner design overview](architecture/README.md).
There are three layers:

```text
Logical planning — ASAPPlanner
    PlanSpace → selected logical Post-ASAP DAG
                         ↓
Deployment compilation — deployment PhysicalPlanCompiler
    Deployment plan: physical DAGs + input bindings + operational configuration
                         ↓
Shared execution — physical operators and DAG runtime
    Bound inputs + physical DAG + run context → results
```

| Layer | Responsibility | Output |
| --- | --- | --- |
| Logical planning | Define legal computations, summary families, parameters and guarantees. | Logical candidates and a selected Post-ASAP DAG. |
| Deployment compilation | Choose a feasible physical realization and establish its inputs, materializations, placement and operational configuration. | A deployment plan containing physical DAGs and their deployment contracts. |
| Shared execution | Run the specified physical computation and coordinate dependencies and resources. | Result streams or an explicit failure. |

`PlanSpace` is the primary output of logical candidate search. Selection and
assembly produce the logical DAG used for deployment compilation. Downstream
physical evidence may inform selection through Planner's interfaces; the flow
does not require choosing a candidate without considering physical feasibility.
Planner-owned semantics and guarantees remain authoritative. See the
[planning workflows](architecture/input-output-workflow.md).

**The deployment decides what to run, where, when and with which inputs. The
shared executor runs that computation.** Scheduling, plan installation,
persistence and serving are deployment activities, not additional planning
layers.

### 3.2 Two computation graphs

| Graph | Describes | Owner |
| --- | --- | --- |
| Logical Post-ASAP DAG | Selected computation semantics, summary operations and shared dependencies. | Planner |
| Physical DAG | Concrete operator choices, configuration, ordered dependencies and input slots. | Shared physical-plan contract, constructed by deployment compilation |

A logical node may expand into several physical operators. Several consumers
may share one physical producer. A materialized intermediate result may become
an input, excluding its upstream computation from that execution.

There is no additional execution IR between these graphs. Exporting a logical
DAG as node IDs and edges is a serialization detail. Loading a physical DAG and
instantiating its operator objects is an execution detail. Neither operation
introduces a new semantic plan.

A deployment plan is a container for physical DAGs and their operational
configuration, not another computation graph. It can hold distinct precompute
and query DAGs that use the same physical operator vocabulary.

### 3.3 Deployment compiler and shared executor

The backend `PhysicalPlanCompiler` is the deployment compilation entry point.
It turns a selected logical Post-ASAP DAG into a feasible deployment contract:

- Lower computation to the shared physical operator vocabulary.
- Select input boundaries and bind them to raw sources or materializations.
- Establish concrete window/state layout, placement and transmission contracts.
- Produce consistent precompute and query plans referencing the same state
  identities and schemas.

The shared library owns operator definitions, validation and implementations.
The compiler uses these contracts to construct physical DAGs; it does not
maintain a separate query or precompute operator hierarchy.

`QueryPlan` contains query identity, input/materialization bindings, a physical
DAG and fallback policy. `PrecomputePlan` contains physical DAGs together with
their trigger, input, state and publication contracts. Catalog and transmission
configuration connect producers and consumers without redefining computation.

At execution time, the deployment resolves input slots to readers or supplied
state and instantiates the specified operators. This step does not reselect
algorithms, change summary semantics or make new deployment decisions. The
executor runs the resulting graph; the deployment handles its results.

## 4. Execution Model

### 4.1 Example: one KLL summary, two quantiles

```text
Precompute DAG                         Query DAG

raw input slot                         stored-state input slot
      ↓                                         ↓
  KLL build                                 KLL merge
      ↓                                    ┌────┴────┐
 state output                              ↓         ↓
                                      p50 readout  p99 readout
```

Logical planning selects KLL computation and the required readouts. Deployment
compilation decides where summaries are built and stored, assigns materialization
identities and defines the input contract for each DAG.

The precompute engine supplies a finite input window, executes the build DAG
and persists its output. The query engine supplies compatible stored states and
executes the query DAG. Merge runs once; both quantile operators independently
consume its output.

Storage retrieval and publication are deployment responsibilities. Build, merge,
readout and dependency execution use the shared library in both engines.

### 4.2 One execution

The executor takes a physical DAG, resolved inputs, selected output roots and
a run context. Before opening sources, it validates the reachable computation
and input contracts. It then creates fresh mutable execution state and returns
output streams.

The deployment drives those streams and handles completion or failure. Multiple
requested streams must be polled concurrently so a slow consumer does not prevent
progress through a shared producer's bounded queues. Dropping one consumer leaves
its siblings active.

The same physical DAG can execute repeatedly. Operator state, consumer cursors
and resource reservations belong to an individual run. Persistent state is
supplied explicitly through deployment inputs rather than implicitly retained
between runs.

## 5. Execution Contracts

| Contract | Invariant |
| --- | --- |
| C1 — Validate before execution | Reject invalid topology, arity, schemas, operator configurations and input boundedness before sources start. |
| C2 — Execute shared producers once | A reachable physical producer executes once per run, regardless of consumer count. |
| C3 — Isolate runs | Independent runs have independent mutable execution state. |
| C4 — Apply backpressure | Bounded communication queues limit retained output; consumers have independent cursors. |
| C5 — Propagate failure and cancellation | Errors are explicit, and cancellation terminates affected work and releases its resources. |
| C6 — Enforce the tracked resource budget | Retained outputs and estimated operator workspace count against the run's byte budget. |

Blocking operators require finite input and finalize after it ends. Unknown
boundedness is insufficient for these operators. Incremental operators may emit
before the input completes. Operator properties must make this distinction
available to validation.

Cancellation is cooperative, including within long computation loops. Individual
synchronous kernel calls limit cancellation responsiveness. Memory accounting
covers tracked allocations, not process RSS or every temporary allocation peak.
Without spill support, exceeding the tracked budget fails the run.

The executor reports errors; deployment policy decides whether to retry or invoke
an explicit fallback. It must not silently replace failed computation or turn an
error into an empty result.

## 6. Inputs and Execution Boundaries

A physical DAG exposes typed input slots. The deployment plan binds each slot
to raw data or an already-computed result.

### 6.1 Raw inputs

A raw input contract identifies the source, schema and required data scope.
The deployment supplies a reader satisfying that contract. Readers declare
boundedness, support cancellation and report schema drift or I/O failures.
Opening readers is deferred until execution validation succeeds.

### 6.2 Materialized inputs

Deployment compilation can replace an upstream computation with a compatible
materialized result:

```text
Logical computation:   Scan → KLL build → merge → readout

Physical query DAG:    stored-state input → merge → readout
```

This boundary is chosen during deployment compilation. The executor receives an
explicit input slot; it does not discover a materialization or decide to omit
upstream work at serving time.

Compatibility includes summary family and parameters, schema, grouping, window
coverage and revision scope. The deployment must establish materialization
readiness and the applicable accuracy guarantee before using the result.
Successful byte decoding or schema matching alone is insufficient.

## 7. Implementation Organization

These are modules within the shared execution library, not architectural layers:

| Module | Owns |
| --- | --- |
| `plan` | Shared physical operator vocabulary, DAG structure, properties and validation |
| `binding` | Instantiate specified operators and resolve supplied input slots |
| `runtime` | Per-run state, streams, shared producers and resource accounting |
| `operators` | Relational and temporal physical implementations |
| `operators/summary` | Summary build, merge and readout implementations |
| `expressions` | Scalar expression evaluation |
| `summary_kernels` | Planner-facing sketch adapters and exact accumulators |
| `sources` | Reader interfaces and input adapters |
| `stored_state` | Supported state decoding, delta application and reconstruction |

Sketch algorithms belong to `asap_sketchlib`. Storage selection and engine
policies belong to deployment repositories. The deployment compiler consumes
the shared operator contracts to lower logical computation.

Keeping the execution library alongside Planner allows one PR to change an IR
operation and its implementation, and enables tests across internal planning
and execution interfaces. Repository location does not transfer deployment
ownership to the planning library.

## 8. Alternatives Considered

### 8.1 Shared ASAP execution — selected

One physical operator vocabulary and executor support both precompute and query
engines. ASAP controls summary-state edges and shared-producer semantics directly.
The cost is maintaining operator correctness, resource management and future
execution optimizations.

Separate operator systems in each deployment would duplicate these responsibilities
and require repeated consistency work. Deployment differences are expressed
through input and operational contracts instead.

### 8.2 DataFusion execution

DataFusion provides established operators and execution infrastructure. Reuse
requires integrating ASAP's summary semantics, state transport, sharing and
lifecycle contracts. Extensions do not inherently require changing DataFusion
core, but optimizations must preserve the supplied semantics.

This design selects native execution, following DataFusion's separation of
operator contracts and implementation responsibilities. It makes no claim of
lower runtime overhead. See the [DataFusion comparison](datafusion-execution-comparison.md).

## 9. Validation

The design requires three levels of automated validation:

| Level | Required behavior |
| --- | --- |
| Operators | Correct results and schemas across supported types, empty/null inputs, errors and resource limits. Summary build/merge/readout preserves the intended state semantics. |
| Physical DAG execution | Correct composition, shared producers, independent consumers, repeated runs, cancellation and resource release. |
| Planning and deployment integration | A selected Post-ASAP DAG compiles to consistent deployment contracts and executes through the shared library with correct input identities and results. |

The KLL example must verify both precompute output and query readouts, including
one merge execution for two consumers. Materialized-input tests must reject
incompatible state and establish that excluded upstream work does not run.
Full-path tests must start with SQL/PromQL and raw inputs, then exercise planning,
deployment compilation and native execution rather than injecting all computed
intermediates.

Existing operator, DAG and resource tests provide a foundation. The weighted
TopK integration exercises a Planner-selected computation with supplied rate
results; raw Scan integration uses a manually constructed DAG. These do not
establish the complete three-layer integration. Automated tests have been run
locally; no separate manual deployment-level verification has been performed.

Deployment acceptance additionally covers real connectors, installation,
window/revision handling, persistence, publication and serving. This document
defines the target design; it does not claim those integrations are complete.

## 10. Limitations and Future Work

The initial execution scope is supported computation with bounded input wherever
blocking operators require it. Logical validity alone does not establish physical
feasibility: deployment compilation must reject an unsupported realization before
committing it, or select an explicit deployment fallback.

Parallelism, sharding, spill, richer ordering/distribution properties and physical
algorithm costing can extend the same physical-plan contract. They must preserve
C1–C6 and the three ownership boundaries. They do not require additional semantic
IR layers.
