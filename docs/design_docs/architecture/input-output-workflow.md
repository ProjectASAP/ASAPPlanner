# ASAPPlanner input, output, and workflows

## Overview

ASAPPlanner is a **logical planning library**. It takes canonical queries and their requirements, explores legal exact and approximate implementations, and returns a logical plan space.

It does **not** deploy or execute plans.

```text
canonical queries + requirements + planning context
                       |
                       v
                  ASAPPlanner
                       |
                       v
              PlanSpace
        (legal Post-ASAP alternatives)
```

If required accuracy, semantic, capability, or cost evidence is unavailable, Planner does not assume it. Unsupported optimizations fail closed, while `KeepPreAsap` preserves exact computation where supported.

---

## Inputs

### Source-language inputs

A frontend converts source-language queries into Planner's canonical representation.

| Input                           |               Required | Purpose                                                  |
| ------------------------------- | ---------------------: | -------------------------------------------------------- |
| Query text                      |                    Yes | Defines the query to plan.                               |
| Query language                  |                    Yes | Selects the appropriate frontend.                        |
| Schema/function catalog         |     Frontend-dependent | Resolves names and types.                                |
| Accuracy requirement            |                    Yes | Use `Exact` when approximation is not allowed.           |
| Query recurrence/time selection | For lifecycle planning | Describes how often and over what period the query runs. |
| Data ingestion interval         |    For PromQL lowering | Defines the selection horizon for bare selectors.        |
| Other workload facts            |               Optional | Enable optimizations that depend on them.                |

Frontend lowering produces one Pre-ASAP `QueryExpr` root for each workload query.

### Canonical Planner inputs

The planning core operates on:

```text
Vec<(query_id, Rc<QueryExpr>, Option<AccuracyTarget>)>
```

The main inputs are:

* **`query_id`** — caller-owned query identity.
* **`QueryExpr`** — canonical exact query semantics.
* **`AccuracyTarget`** — required end-to-end accuracy, if enforced.
* **Strategies** — candidate transformations Planner may explore.
* **`AccuracyModel`** — determines how guarantees compose and satisfy targets.
* **`CostModel`** — evaluates candidate cost and availability.
* **`AccuracyEvidenceProvider`** — provides facts required to prove candidate guarantees.

Most integrations should use the standard strategy set and `DefaultAccuracyModel`.

### Additional inputs for lifecycle planning

Lifecycle-aware planning compares maintaining a summary against recomputing the raw query.

It additionally requires:

* query workload bindings;
* planning time and horizon;
* query recurrence;
* data arrival/update rate;
* deployment lifecycle capabilities; and
* comparable cost information for summary and raw execution.

Missing required information remains **unknown** rather than being treated as zero.

---

## Evidence

**Evidence is a scoped fact used to establish legality, accuracy, cost, or feasibility.**

Examples include:

| Evidence        | Examples                                                         |
| --------------- | ---------------------------------------------------------------- |
| Semantic/domain | Input range, nonempty population, nonzero denominator            |
| Accuracy        | Quantile domain, Top-K confidence, composition certificate       |
| Cost            | CPU time, operation count, memory, scan/storage I/O              |
| Workload        | Cardinality, distribution, ingestion rate, recurrence            |
| Capability      | Supported summaries, merge/delete support, window implementation |

Evidence must apply to the relevant workload and implementation. Time-sensitive evidence should also carry freshness information.

When required evidence is missing, the dependent optimization is unavailable.

---

## Outputs

### `PlanSpace<Id>`

`PlanSpace` is Planner's canonical output. It contains:

* canonical workload roots;
* memo groups for discovered target sub-DAGs;
* legal replacement candidates;
* rejected candidates and reasons; and
* information needed for cross-group selection.

A `PlanSpace` represents a **space of logical DAG choices**, not a single executable plan.

`PlanSpace::cost_sorted` provides a ranked view for inspection:

```text
Vec<RankedGroup {
    target,
    consumer_count,
    candidates,
    costs,
}>
```

This view is useful for debugging, explanation, or downstream optimization. Candidate presence does not imply physical deployability.

### Selected Post-ASAP DAG

`PlanSpace::global_selection*` coordinates decisions across memo groups.

```text
PlanSpace
    |
    v
global_selection
    |
    v
materialize(root)
    |
    v
Post-ASAP DAG
```

The resulting DAG records logical information such as summary operators, parameters, schemas, windows, and accuracy guarantees.

It is still **not an executable deployment plan**. Physical operator binding, placement, storage, and execution remain downstream responsibilities.

### Lifecycle-aware output

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

### 2. Select a logical DAG

Use when Planner should coordinate sharing and composition across the workload.

```text
PlanSpace
    -> global_selection
    -> materialize
    -> selected Post-ASAP DAGs
```

This produces structurally compatible logical plans. It does not determine whether maintaining summaries is cheaper than raw execution.

### 3. Make a lifecycle-aware decision

Use when deciding whether maintained summary state should actually be deployed.

```text
PlanningWorkload + Pre-ASAP roots
    -> search_workload_with_targets
    -> global_selection_with_summary_maintenance_lifecycles
    -> materialize_with_summary_maintenance_lifecycles
    -> lifecycle-aware Post-ASAP plans
    -> downstream physical deployment
```

This workflow considers recurrence, data arrival, planning horizon, capabilities, and comparable summary/raw costs.

It is the recommended workflow for deployment decisions.

---

## Replanning

Replanning uses the same interface as initial planning.

Run Planner again whenever a selection-relevant input changes, such as:

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

Planner produces a new logical decision. The downstream system owns deployment diffing, migration, activation, and rollback.

---

## Integration guidance

For normal integrations:

1. Lower the complete workload through a frontend.
2. Call a whole-workload `search_workload*` API.
3. Use the standard strategy set unless extending Planner.
4. Preserve explicit accuracy requirements and required evidence.
5. Use lifecycle-aware selection before making deployment cost decisions.
6. Treat physical compilation and execution as downstream responsibilities.

Lower-level Planner traits and APIs are extension points for research and deployment-specific customization; they are not separate required workflow stages.

## Related documents

* [Planner pipeline](../concepts/planner-pipeline.md)
* [Pre-ASAP IR](../concepts/pre-asap-ir.md)
* [Post-ASAP IR](../concepts/post-asap-ir.md)
* [Planner/downstream boundary](planner-downstream-boundary.md)
* [Plan search internals](asap-aware-plan-search.md)
* [Public library reference](../../develop_docs/library-api.md)
