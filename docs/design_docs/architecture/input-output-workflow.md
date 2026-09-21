# ASAPPlanner input, output, and workflows

## Overview

ASAPPlanner is a **logical planning library**. Its input is a planning workload
plus the models, evidence, and deployment capabilities needed by the requested
planning workflow. Its canonical output is a `PlanSpace` containing the legal
Post-ASAP alternatives for the workload.

### Input fields at a glance

| Input | Fields | Required |
|---|---|---:|
| `PlanningWorkload.query_workload` | Query language and one-time/repeating query workloads | Yes |
| `PlanningWorkload.data_workload` | Data arrival and optional evidence about ingestion, cardinality, and distribution | No implicit default. Set `None` when unavailable for non-PromQL workloads; PromQL requires `Some(DataWorkload)` with a nonzero ingestion interval. |
| Frontend-specific dependencies (outside `PlanningWorkload`) | `SqlCatalog` for SQL; `now_ms` and, when needed, `HistogramCatalog` for PromQL | `SqlCatalog` is required for SQL lowering; `now_ms` is required for PromQL lowering |
| Planning models | Candidate cost/ranking and accuracy composition/checking | Used by the relevant APIs; built-in `DefaultCostModel` and `DefaultAccuracyModel` are available |
| External evidence and capabilities | Domain facts, measured costs, workload statistics, and runtime support | Supply when available and when the chosen optimization or lifecycle decision depends on them; absence is not proof |

As part of the planning workflow, frontend lowering converts the workload
entries into canonical Pre-ASAP `QueryExpr` roots. Those roots and the
candidate-search API that consumes them are internal stages, not additional
end-to-end user inputs. The SQL data catalog is a separate frontend input,
not a field of `PlanningWorkload`; the other frontend dependencies are listed under
[Frontend-specific dependencies](#frontend-specific-dependencies).

### Output at a glance

| Output | Fields or contents | Meaning |
|---|---|---|
| `PlanSpace<Id>` | The legal candidate Post-ASAP DAGs for the workload, represented compactly as canonical roots, one candidate set per target sub-DAG, and cross-target composition information | The ASAPPlanner output |

[Ranking](#ranked-view), [selection and
DAG assembly](#selection-and-dag-assembly), and
[lifecycle](#lifecycle-aware-helper) APIs operate on this `PlanSpace`.
`PlanSpace` contains logical candidate DAGs; it does **not** choose whether
to build, maintain, or recompute their summary state. Using it without the
lifecycle helper is appropriate for candidate inspection or when a downstream
system makes its own deployment decision. `global_selection` plus
`assemble_selected_dag` yields a selected logical DAG, not a recommendation to maintain
its summaries. For a Planner-side maintenance-versus-recompute decision, use
the two-stage lifecycle-aware workflow. First,
`global_selection_with_summary_maintenance_lifecycles` selects compatible
candidates across the `PlanSpace` and returns a `GlobalSelection`. Then,
`materialize_with_summary_maintenance_lifecycles` builds the DAG for one root
and returns a `SummaryMaintenanceLifecyclePlan` with that root and lifecycle
decisions. This is the recommended path before deployment when Planner is
responsible for that cost comparison. Lifecycle is separate because its
decision needs a horizon, update rate, and physical-cost/capability facts that
logical candidate search does not necessarily have; it is not an intrinsic
property of a candidate DAG.

The candidate DAGs are logical planning artifacts. ASAPPlanner does **not**
produce a deployed executable plan; downstream systems bind physical operators,
choose placement and storage, deploy state, and execute queries.

```text
PlanningWorkload + frontend-specific dependencies
                        |
                        v
                   ASAPPlanner
                        |
                        v
        PlanSpace: candidate Post-ASAP DAGs
```

If required accuracy, semantic, capability, or cost evidence is unavailable, Planner does not assume it. Unsupported optimizations fail closed, while `KeepPreAsap` preserves exact computation where supported.

---

## Running example: a recurring PromQL query

Suppose a dashboard evaluates `count_over_time(up[5m])` once a minute, and
`up` receives a sample every 15 seconds. This diagram traces the concrete
inputs and the three possible uses of the same candidate space:

```mermaid
flowchart TD
    Q["query_workload: PromQL; repeating query count_over_time(up[5m]); every 60 s; exact; real-time; predictable"]
    D["data_workload: continuous arrival; declared ingestion interval 15 s"]
    T["Frontend argument: now_ms"]
    F["PromQL lowering"]
    R["One canonical QueryExpr root"]
    S["Candidate search"]
    P["PlanSpace: logical choices for this root"]
    I["cost_sorted: inspect choices"]
    G["global_selection + assemble_selected_dag(root)"]
    L["One selected Post-ASAP DAG; exact KeepPreAsap if no optimization is selected"]
    X["Extra lifecycle inputs: horizon; update rate; capabilities; comparable summary/raw costs"]
    H["Lifecycle-aware global selection"]
    HM["Materialize one root and decide lifecycle"]
    O["SummaryMaintenanceLifecyclePlan: materialized root + maintenance/recompute decision"]
    B["Backend: bind and execute an accepted contract"]
    Q --> F
    D --> F
    T --> F
    F --> R --> S --> P
    P --> I
    P --> G --> L --> B
    P --> H
    X --> H --> HM --> O --> B
```

“Predictable” says the query is known in advance; it is independent of its
one-minute recurrence. The `PlanSpace` may contain an exact count-summary
realization, but it is not a deployed query. Without the extra lifecycle
inputs, the caller can still inspect candidates or obtain a logical DAG; it
cannot conclude that maintaining a summary is cheaper than recomputing raw
results.

For contrast, a one-time SQL query needs a catalog but need not supply data
arrival evidence merely to inspect logical alternatives:

```mermaid
flowchart LR
    Q["query_batch: SELECT COUNT(*) FROM metrics; invocations 1; AdHoc"]
    C["SqlCatalog: resolves metrics and its columns"]
    F["SQL lowering"]
    R["One QueryExpr root"]
    P["Candidate search → PlanSpace"]
    Q --> F
    C --> F
    F --> R --> P
```

In this SQL example, `data_workload` can be `None` if the chosen lowering and
search rules do not consume it. The lifecycle helper is not needed merely to
inspect the `PlanSpace`.

---

## Inputs

### `PlanningWorkload`

A frontend receives one `PlanningWorkload`. Query demand and facts about the
queried data are separate because they have different sources and update
cycles:

```rust
struct PlanningWorkload {
    query_workload: QueryWorkload,
    data_workload: Option<DataWorkload>,
}
```

| Field | Required | Purpose |
|---|---:|---|
| `query_workload` | Yes | Contains the source language and every one-time or repeating query. |
| `data_workload` | Optional generally; required for PromQL | There is no automatic default: explicitly set `None` when unavailable for non-PromQL workloads. PromQL requires `Some(DataWorkload)` with a nonzero `data_ingestion_interval`. |

#### `query_workload: QueryWorkload`

`QueryWorkload` describes query demand. It deliberately does not describe
whether source data is still arriving.

```rust
struct QueryWorkload {
    language: QueryLanguage,
    query_batch: Option<Vec<BatchEntry>>,
    repeating_queries: Option<Vec<RepeatingEntry>>,
}
```

| Field | Required | Purpose |
|---|---:|---|
| `language` | Yes | Source language shared by every entry: PromQL, a SQL dialect, DataFusion, or Elastic DSL. It selects the frontend. |
| `query_batch` | Optional | Finite one-time query entries. `None` means there is no batch portion. |
| `repeating_queries` | Optional | Recurrent query entries. `None` means there is no repeating portion. |

Both entry collections may be present. `QueryWorkload::entries()` normalizes
them into one ordered stream: batch entries first, followed by repeating
entries. If both are absent, lowering produces no query roots.

##### `query_batch: Option<Vec<BatchEntry>>`

```rust
struct BatchEntry {
    query: Query,
    requirements: QueryRequirements,
    predictability: Predictability,
    invocations: u64,
    execute_at: Option<TimestampMs>,
    time_selection: TimeSelection,
}
```

| Field | Required | Purpose | Why it matters / example |
|---|---:|---|---|
| `query` | Yes | Raw query text in `QueryWorkload.language`. | `count(up)` determines the expression to lower and plan. |
| `requirements` | Yes | Accuracy and response-latency requirements. Defaults mean exact accuracy and unspecified latency. | An explicit ε target permits approximate candidates; the exact default does not. |
| `predictability` | Yes as a field; `Unknown` is allowed | Whether the query is ad hoc, known in advance, or unknown. `known_at` records when a predictable query became known. | A report known at 10:00 and scheduled for 11:00 may use a `Prepared` summary before execution. `AdHoc` or `Unknown` does not establish that eligibility. |
| `invocations` | Yes, nonzero | Number of executions in this finite batch. | Ten executions can amortize one summary build differently from one execution. |
| `execute_at` | Optional | Known execution time. | The `Prepared` case above also needs an execution time; without it Planner cannot establish a preparation window. |
| `time_selection` | Yes | Whether the query follows current data or a historical interval, its lookback, and any fixed upper bound. | A moving five-minute window can require deletion/window support that a fixed historical interval does not. |

##### `repeating_queries: Option<Vec<RepeatingEntry>>`

```rust
struct RepeatingEntry {
    query: Query,
    demand: RepeatedDemand,
    requirements: QueryRequirements,
    predictability: Predictability,
    time_selection: TimeSelection,
}
```

| Field | Required | Purpose | Why it matters / example |
|---|---:|---|---|
| `query` | Yes | Raw query text in `QueryWorkload.language`. | `rate(up[5m])` determines the expression to lower and plan. |
| `demand` | Yes | A nonzero fixed interval, fixed interval with evaluation phase, nonempty explicit schedule, or evidence-backed estimated rate. | A query every minute produces more expected reads over a horizon than one every hour. |
| `requirements` | Yes | Accuracy and response-latency requirements. | An exact dashboard query cannot use an approximate summary solely because it is cheaper. |
| `predictability` | Yes as a field; `Unknown` is allowed | Records whether future executions are known in advance; independent of recurrence. | Current lifecycle code does not use this field for repeating entries; set `Unknown` if no predictability claim is available. |
| `time_selection` | Yes | Event-time scope, optional lookback, and optional fixed `as_of` time. | A live five-minute lookback differs from a fixed historical range when checking maintenance capabilities. |

##### Shared entry fields

`BatchEntry` and `RepeatingEntry` both contain `QueryRequirements`,
`Predictability`, and `TimeSelection`. The nested requirement and time-selection
fields expand as follows:

| Structure | Field | Meaning |
|---|---|---|
| `QueryRequirements` | `accuracy` | `Explicit(AccuracyTarget)` or `ImplicitExact`. Use an explicit target when approximation is allowed. |
| `QueryRequirements` | `response_latency` | An optional finite, non-negative maximum in milliseconds. The default is unspecified. |
| `TimeSelection` | `scope` | `RealTime`, `Longitudinal`, `Mixed`, or `Unknown`. |
| `TimeSelection` | `lookback` | Optional event-time duration selected before the upper bound. |
| `TimeSelection` | `as_of` | Optional fixed upper-bound timestamp; `None` means planning/evaluation time. |

Frontend lowering produces one Pre-ASAP `QueryExpr` root for each normalized
query entry. The caller must retain each root's association with its workload
entry for later recurrence and lifecycle planning.

#### `data_workload: Option<DataWorkload>`

`DataWorkload` describes the data being queried. Each empirical field uses
`Evidence<T>` so a value is accompanied by its source and freshness.

```rust
struct DataWorkload {
    arrival: DataArrival,
    data_ingestion_interval: Evidence<DurationMs>,
    ingestion_volume: Evidence<u64>,
    ingestion_rate: Evidence<Rate>,
    input_cardinality: Evidence<u64>,
    distribution: Evidence<DataDistribution>,
}
```

| Field | Required | Purpose and behavior when unavailable |
|---|---:|---|
| `arrival` | Present; may be `Unknown` | Distinguishes data at rest, continuously ingesting data, and mixed data. Continuous-maintenance decisions are limited when unknown. |
| `data_ingestion_interval` | Required and nonzero for PromQL; otherwise optional | Sampling cadence for each PromQL source. Bare instant selectors use it as their explicit selection horizon. |
| `ingestion_volume` | Optional evidence | Total ingestion volume when known. Dependent resource estimates remain unavailable when absent or stale. |
| `ingestion_rate` | Optional evidence | Updates per second used to price continuous maintenance. It must be finite and non-negative; at-rest data cannot declare a positive rate. |
| `input_cardinality` | Optional evidence | Input row/sample count used by applicable sizing, accuracy, or cost rules. |
| `distribution` | Optional evidence | `Zipf`, `Uniform`, or `Bursty` key distribution used only by rules that explicitly consume it. |

##### `Evidence<T>` fields

Every empirical field in `DataWorkload` uses `Evidence<T>`, which has four
fields:

| Field | Meaning |
|---|---|
| `value: Option<T>` | The fact itself; `None` means unavailable. |
| `source: EvidenceSource` | `Declared`, `Observed`, `Derived`, or `Unknown`. |
| `observed_at_ms: Option<u64>` | Observation timestamp used for freshness checks. |
| `valid_for_ms: Option<u64>` | Validity duration. A duration without an observation timestamp is unusable. |

Unavailable or stale data evidence stays unknown. Planner does not reinterpret
it as zero ingestion, zero cardinality, or a favorable distribution.

### Frontend-specific dependencies

These inputs are supplied alongside `PlanningWorkload`, rather than nested
inside it:

| Frontend | Additional lowering inputs |
|---|---|
| PromQL | `now_ms`, plus an optional `HistogramCatalog` when histogram semantics must be resolved |
| SQL | `SqlCatalog`; single-query APIs also receive the accuracy target and optionally an explicit SQL dialect |
| MetricsQL | No catalog; the single-query API receives the query text and accuracy target directly |

They are explicit frontend function arguments, not one generic
`FrontendContext` type.

### Planning evidence inputs

**A model is planning logic; evidence is a scoped fact used by that logic.**
For example, `DefaultAccuracyModel` knows how to combine error bounds, while
an input-domain observation tells it whether a particular quantile ratio has
the required bound. The former can be provided by a built-in default; the
latter cannot be fabricated by one.

| Input | Where it enters / default | Why it matters |
|---|---|---|
| Query demand | `PlanningWorkload.query_workload` is required. | Query text and requirements define the semantics; recurrence determines how often a selected plan is read. |
| Data workload | `PlanningWorkload.data_workload` may be `None` except for PromQL, which requires a nonzero ingestion interval. Other empirical fields may be unknown. | Arrival rate and cardinality can change maintenance cost or accuracy calculations; unknown values cannot be treated as zero. |
| Accuracy model | Target-aware search takes an `AccuracyModel`; `DefaultAccuracyModel` is available. Default strategies also use it for candidate construction. | Composes candidate guarantees and checks them against requested accuracy. The model does not itself provide missing data-domain facts. |
| Cost model | Candidate strategies and `cost_sorted`/`global_selection` use a `CostModel`; `DefaultCostModel` is available. | Ranks or selects candidates. The built-in model is not a measured deployment cost for every physical implementation. |
| Accuracy/domain evidence | `AccuracyEvidenceProvider`; default strategies use `NoAccuracyEvidence` when no provider is supplied. | Input ranges, nonempty populations, Top-K intervals, and similar facts can certify or rule out particular approximations. Missing facts remain unknown. |
| Measured cost evidence | Supplied through a deployment-specific cost model or physical-evidence provider when cost-based physical/lifecycle comparison is needed. | CPU, memory, and I/O estimates must be comparable before claiming a summary beats raw recomputation. |
| Runtime/lifecycle capabilities | Passed to lifecycle APIs or checked by deployment-specific providers; `SummaryMaintenanceLifecycleCapabilities::default()` enables all four lifecycle shapes, so it is not proof of actual backend support. | Prevents choosing a maintenance/window operation the intended executor cannot implement. |

Evidence must apply to the relevant workload and implementation. Time-sensitive evidence should also carry freshness information.

The models and query demand are important inputs in ordinary planning, even
when a caller chooses built-in model defaults. External evidence and runtime
capabilities matter just as much to decisions that use them, but their values
are not universally required to construct `PlanSpace`. Missing evidence does
not invalidate unrelated candidates; a dependent decision remains unknown or
unavailable rather than using a favorable assumption.
Planner output may record the resulting guarantee, evidence provenance, or a
rejection reason, but evidence itself remains an input.

This completes the canonical input boundary for producing `PlanSpace`.
Lifecycle-specific values such as a planning horizon and deployment lifecycle
capabilities are not additional `PlanSpace` inputs. They are parameters to the
optional [lifecycle-aware helper](#lifecycle-aware-helper) described after the
output.

---

## Output

### `PlanSpace<Id>`

`PlanSpace` is Planner's canonical output. It contains:

* canonical workload roots;
* one `TargetSubDAGCandidates` entry for each discovered target sub-DAG;
* legal replacement candidates;
* rejected candidates and reasons; and
* information needed to select compatible candidates across targets.

A `PlanSpace` represents a **space of logical DAG choices**, not a single executable plan.

This public interface is intended for library integrators such as
ASAPQuery-backend. Users submitting queries through those systems do not need
to handle it. Under the current division of responsibilities, the backend
owns the physical deployment decision: it can inspect candidates and select
among them using its supported implementations, measured costs, and available
resources. An integration making that selection itself needs access to the
candidate space. `assemble_selected_dag(root)` connects choices after
selection, so it does not replace this candidate interface.

This boundary follows who owns selection. A higher-level API could instead
accept the backend's models and capabilities, select internally, and return
only selected DAGs; exposing `PlanSpace` is not inherently required by the
existence of deployment constraints.

Why does search return a space rather than one “optimal” plan? The search API
does not receive one universally comparable physical cost and capability model
for every deployment. For example, a summary that is cheap to update in one
backend may be expensive or unsupported in another; the answer can also change
when a query is run once versus every minute over a long horizon. Returning
only one candidate at this stage would discard alternatives before those
deployment facts are known. This is the current API boundary, not a claim that
users should manually pick arbitrary DAG fragments: callers with a suitable
model can use `global_selection` and, when deciding maintain versus recompute,
the lifecycle-aware workflow to obtain a selected plan. Downstream still
checks and commits its physical implementation.

It represents that space compactly instead of eagerly copying every complete
DAG. `PlanSpace` stores the workload's canonical roots once, creates one
`TargetSubDAGCandidates` entry for each distinct target sub-DAG, and stores
that target's replacement alternatives once inside the entry. Candidate
children refer back to canonical
targets, so common subexpressions and shared alternatives are not duplicated
across roots.

For example, if one target has three alternatives and its child has two,
eager enumeration could create six complete DAGs. `PlanSpace` stores the three
parent alternatives, the two child alternatives, and their relationship.
Whole-plan selection chooses compatible alternatives across those targets;
materialization then recursively substitutes the selected alternatives to
construct a complete Post-ASAP DAG. Sharing each target's candidate set avoids the
Cartesian-product expansion of complete DAGs and preserves shared nodes.

The remaining APIs in this section derive information from that one output;
they do not define separate ASAPPlanner output contracts.

### Ranked view

`PlanSpace::cost_sorted` returns one `RankedTargetSubDAGCandidates` for each
`TargetSubDAGCandidates` entry. Conceptually, it is the same target's
alternatives in cost-model preference order where the model defines one
(otherwise discovery order), with one displayed cost per alternative. It is
a **view of one decision point**, not a complete DAG or a selected plan.

Here, `target` is the canonical `Rc<QueryExpr>` for the sub-DAG being replaced.
A workload `root` is the top-level `QueryExpr` for a submitted query; every
root is a target, but a target can also be an inner expression. For example,
in `count(up) + 1`, the whole addition is a root and the inner `count(up)`
can be a separate target with its own candidates.

The return type is `Vec<RankedTargetSubDAGCandidates<'_>>`; each element has this shape:

```rust
struct RankedTargetSubDAGCandidates<'a> {
    target: &'a Rc<QueryExpr>,
    consumer_count: usize,
    candidates: Vec<&'a ReplacementSubDAG>,
    costs: Vec<f64>, // costs[i] describes candidates[i]
}
```

`consumer_count` counts references to this target in the workload. `costs`
may contain `NaN` when the model has no numeric estimate; the ordering is not
itself a deployment-cost certificate. This view is useful for debugging,
explanation, or downstream optimization. Candidate presence does not imply
physical deployability.

### Selection and DAG assembly

Use this workflow when the caller wants Planner to turn its candidate space
into a selected logical plan for each workload query. The input is `PlanSpace`
and a cost model. The output is a set of Post-ASAP DAGs, one per query root,
with shared nodes where the selected plans reuse the same computation.

The caller uses **two public APIs**: call
`PlanSpace::global_selection(&cost_model)` once to obtain a `GlobalSelection`,
then call `GlobalSelection::assemble_selected_dag(root)` for each wanted query
root. For one query this is two function calls; for N query roots it is one
selection call followed by N assembly calls. There is no single combined
search/selection/assembly call in this workflow.

Selection chooses compatible alternatives across the workload. For example,
if two queries can share a summary, their choices must agree on the shared
computation. `assemble_selected_dag(root)` then connects the chosen alternatives
into each query's DAG in memory, preserving shared nodes. Candidate search has
already built candidate sub-DAGs; assembly connects the selected choices into
the result for one query root.

| Input → decisions → output (click a step for details) |
|:---:|
| **Input:** [PlanSpace](asap-aware-plan-search.md) + cost model |
| ↓ |
| **Select:** [global_selection](../../develop_docs/library-api.md#what-does-global-selection-mean) chooses compatible alternatives |
| ↓ |
| **Assemble:** [assemble_selected_dag(root)](../../develop_docs/library-api.md#api-definition-and-example) connects those choices for each query root |
| ↓ |
| **Output:** one selected logical [Post-ASAP DAG](../concepts/post-asap-ir.md) per query root |

Each output DAG specifies the chosen operators, parameters, and accuracy
guarantees. Its root is represented by `Rc<SummaryNode>`; the
[API reference](../../develop_docs/library-api.md#api-definition-and-example)
describes the function signatures and return handling.

This workflow selects how to compute the query. To also decide whether to
maintain summary state or recompute raw data, use the
[lifecycle-aware workflow](#lifecycle-aware-helper) below. The downstream
system binds physical implementations, deploys state, and executes the plan.

### Lifecycle-aware helper

Lifecycle-aware planning is a two-call workflow on an existing `PlanSpace`, not
part of the canonical input-to-`PlanSpace` operation:

1. `global_selection_with_summary_maintenance_lifecycles` uses the workload
   binding, lifecycle capabilities, and comparable costs to choose compatible
   candidates across target sub-DAGs. It returns `GlobalSelection`, not a DAG or a
   deployment plan.
2. For each wanted query root, `materialize_with_summary_maintenance_lifecycles`
   takes that selection and root, constructs a Post-ASAP DAG, compares the
   selected summary's maintenance cost with raw recomputation, and returns
   `Option<SummaryMaintenanceLifecyclePlan>`. When a summary does not beat a
   known raw cost, or a required comparable cost is unavailable, the result
   retains the exact `KeepPreAsap` root and no summary deployments.

The second call is per root; a workload with multiple roots can therefore
produce multiple lifecycle plans from one `GlobalSelection`.
See the [library guide's lifecycle and capabilities section](../../develop_docs/library-api.md#lifecycle-and-capabilities)
for an API example and the capability contract.

Across the two calls, the caller supplies these parameters:

| Helper parameter | Source | Required |
|---|---|---:|
| `PlanSpace` | Canonical ASAPPlanner output; passed to selection | Yes |
| `GlobalSelection` and one root | Selection result and a root in that `PlanSpace`; passed to materialization | Yes for each materialized root |
| Workload binding | `QueryWorkload` plus the workload-entry indices associated with each root | Yes |
| Planning time (`now_ms`) | Caller clock in Unix milliseconds | Yes |
| Planning horizon | Caller policy | Conditional: required for finite totals over recurring demand |
| Data arrival and update rate | `DataWorkload` evidence | Conditional: required to cost continuous maintenance |
| Lifecycle capabilities | Deployment/runtime provider | Yes for checking deployable lifecycle alternatives |
| Summary and raw cost information | Cost model and physical-evidence provider | Yes for a cost-based maintenance-versus-recompute decision |

Recurrence and time selection are already fields of the bound `QueryWorkload`;
they are not duplicated as separate top-level inputs. Similarly, data arrival
and update rate are read from the optional `DataWorkload`. Missing required
facts remain unknown rather than being treated as zero.

`SummaryMaintenanceLifecyclePlan` additionally records:

* the materialized root;
* lifecycle choices for summary state;
* planning horizon and expected reads/updates;
* selected window implementation and guarantees;
* comparable summary and raw-recomputation costs; and
* whether raw recomputation was selected.

---

## Workflows

### 1. Inspect logical alternatives

Use when the caller wants to inspect Planner's candidate space or perform physical optimization downstream.

```text
PlanningWorkload
    -> frontend lowering
    -> search_workload_with_targets
    -> PlanSpace
    -> cost_sorted (optional)
```

This exposes legal logical alternatives but does not choose a deployment.

### 2. Select logical DAGs for the workload

Use when Planner should coordinate sharing and composition across the workload.

```text
PlanSpace with N query roots
    -> global_selection(cost_model), once for the workload
    -> one GlobalSelection
    -> assemble_selected_dag(root), once for each query root
    -> N selected Post-ASAP DAG roots, with shared nodes where applicable
```

Each successful assembly call returns one DAG root. The caller collects those
roots to obtain the workload's selected DAGs; a single-query workload yields
one root. This is the same workflow as
[Selection and DAG assembly](#selection-and-dag-assembly), shown at workload
scope. It does not determine whether maintaining summaries is cheaper than raw
execution.

### 3. Make a lifecycle-aware decision

Use when deciding whether maintained summary state should actually be deployed.

```text
PlanSpace + lifecycle inputs
    -> global_selection_with_summary_maintenance_lifecycles
    -> GlobalSelection
    -> materialize_with_summary_maintenance_lifecycles(selection, root, ...)
    -> SummaryMaintenanceLifecyclePlan for that root
    -> downstream physical deployment
```

These calls consider recurrence, data arrival, planning horizon, capabilities,
and comparable summary/raw costs. They do not change the canonical
`PlanningWorkload -> PlanSpace` interface.

It is the recommended workflow for deployment decisions.

---

## Replanning (future support)

> **Status: Future support.** ASAPPlanner does not currently define an
> end-to-end replanning or deployment-transition contract.

The intended design will reuse the initial-planning input boundary. A caller
would request replanning whenever a selection-relevant input changes, such as:

* query semantics or accuracy requirements;
* recurrence, horizon, data arrival, or distribution;
* evidence;
* supported capabilities;
* cost calibration; or
* available materialized state.

```text
updated workload + evidence + capabilities
                    |
                    v
              run Planner
                    |
                    v
          new PlanSpace / selection
                    |
                    v
       downstream deployment transition
```

Today, a caller can run Planner again and obtain a new logical candidate space,
but ASAPPlanner does not relate that result to the previous plan. Future support
must define plan identity and compatibility across runs. Deployment diffing,
migration, activation, and rollback remain downstream responsibilities.

## Related documents

* [Planner pipeline](../concepts/planner-pipeline.md)
* [Pre-ASAP IR](../concepts/pre-asap-ir.md)
* [Post-ASAP IR](../concepts/post-asap-ir.md)
* [Planner/downstream boundary](planner-downstream-boundary.md)
* [Plan search internals](asap-aware-plan-search.md)
* [Public library reference](../../develop_docs/library-api.md)
