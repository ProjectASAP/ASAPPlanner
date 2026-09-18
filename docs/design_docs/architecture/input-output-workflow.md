# ASAPPlanner input, output, and workflow

## Purpose

This document defines the public mental model for embedding ASAPPlanner. It
answers three questions:

1. What must a caller provide?
2. What does Planner return?
3. Which workflow should a caller use?

ASAPPlanner is a **logical planning library**. It accepts canonical query roots
and their requirements, explores legal ways to answer them with exact or
approximate summaries, and returns the resulting candidate space. It does not
deploy or execute a plan.

The central contract is:

```text
canonical query roots + requirements + optional planning context
                              |
                              v
                        ASAPPlanner
                              |
                              v
             PlanSpace: legal Post-ASAP alternatives
```

Candidate discovery, local replacement strategies, memo groups, accuracy
allocation, and ranking are internal stages of this contract. They are public
Rust extension points, but an application does not need to call them one by one.

## The boundary in one example

For a recurring PromQL quantile query, the caller:

1. lowers the source query to a canonical Pre-ASAP `QueryExpr`;
2. associates that root with an application query ID and accuracy requirement;
3. asks Planner to search the workload; and
4. receives a `PlanSpace` containing the exact alternative and every proven
   legal summary alternative.

A downstream system may inspect all alternatives or ask Planner's selection
helpers to choose and materialize a Post-ASAP DAG. The downstream system still
binds that logical DAG to executable operators, storage, and placement.

If a proof or cost is missing, Planner does not invent it. The affected
optimization is absent, rejected, or ranked as unavailable; an exact
`KeepPreAsap` alternative preserves the original computation where supported.

## Input contract

There are two input layers. A frontend translates source-language input into
the canonical input accepted by the planning core.

### Source-language input

| Input | Required | Meaning if omitted or unknown |
|---|---:|---|
| Query text | Yes | There is no query to plan. |
| Query language | Yes | The caller must select the matching frontend. |
| Schema/function catalog | Frontend-dependent | SQL name and type resolution fails without the required catalog. |
| Per-query accuracy requirement | Yes | Use `Exact` explicitly when approximation is not allowed. |
| Query recurrence and time selection | Required for lifecycle-aware comparison | Candidate discovery can proceed, but Planner cannot compare repeated maintenance with raw recomputation over time. |
| Data ingestion interval | Required by PromQL workload lowering | Bare selectors have no defensible selection horizon without it. |
| Other data-workload facts | Optional | Optimizations requiring an unknown fact remain unavailable; unknown never means zero. |

The normalized workload types are `PlanningWorkload`, `QueryWorkload`, and
optional `DataWorkload`. Frontend output is one Pre-ASAP `QueryExpr` root per
workload entry. The caller must retain the association between each root and
its workload entry.

### Canonical planning input

The planning core consumes:

```text
Vec<(query_id, Rc<QueryExpr>, Option<AccuracyTarget>)>
```

The fields have these roles:

| Field | Required | Role |
|---|---:|---|
| `query_id` | Yes | Caller-owned identity used to associate results with queries. |
| `QueryExpr` | Yes | Canonical exact query semantics; this is the Pre-ASAP root. |
| Root `AccuracyTarget` | Recommended; required for an enforced end-to-end target | Filters out summary candidates whose final guarantee is missing or insufficient. `None` means that this call adds no root-level requirement. |
| Strategy set | Yes in the configurable API | Use `default_strategies_with_evidence` for the standard set with deployment models. Custom sets are an extension mechanism, not separate workflow stages. |
| `AccuracyModel` | Yes in the configurable API | Defines how guarantees compose and whether they satisfy a target. Most callers use `DefaultAccuracyModel`. |
| `CostModel` | Required for ranking or selection | Reports candidate availability and preference. The built-in default is useful for inspection, not a claim about deployment cost. |
| `AccuracyEvidenceProvider` | Optional in general; required by candidates whose proof needs it | Supplies typed facts such as certified quantile input domains or Top-K separation. Without a required proof, that candidate fails closed. |

### Inputs used for lifecycle-aware selection

Lifecycle-aware selection answers a narrower question: is it cheaper to build,
maintain, and read a summary, or to execute the raw query for the declared
workload?

| Input | Required for lifecycle-aware selection | Role |
|---|---:|---|
| Query workload binding | Yes | Identifies which workload entries consume each root. |
| Planning time (`now_ms`) | Yes | Evaluates schedules and evidence freshness. |
| Planning horizon | Required for finite recurring totals | Bounds the number of reads and updates. Missing horizon leaves some totals unknown. |
| Data arrival and update rate | Required for continuously maintained cost | Prices maintenance work. Unknown values cannot be treated as no updates. |
| Lifecycle capabilities | Yes | Declares which lifecycle alternatives the deployment can implement. |
| Complete comparable cost inputs | Required to choose on cost | Both the summary path and raw baseline must be costed in the same scope. |

Lifecycle information is therefore not an optional accuracy check. It is
optional only when the caller wants candidate discovery or a structural
selection rather than a workload-costed deployment decision.

## What “evidence” means

**Evidence is a scoped fact used to justify candidate legality, accuracy, or
cost.** It may be declared by a contract, derived analytically, measured in an
offline benchmark, or observed online. Evidence must identify the workload and
implementation to which it applies and, when time-sensitive, carry freshness
information.

| Kind | Examples | Consequence when required but missing |
|---|---|---|
| Semantic or domain evidence | Finite input range, nonempty population, denominator excludes zero | The transformation cannot prove its preconditions. |
| Accuracy evidence | Quantile domain, Top-K confidence margins, composition certificate | The approximate candidate has no valid end-to-end guarantee. |
| Cost evidence | CPU time, operation count, retained/peak bytes, scan or storage I/O | Planner cannot make the corresponding cost comparison. |
| Workload evidence | Cardinality, distribution, ingestion rate, recurrence | Dependent sizing, propagation, or lifecycle costs remain unknown. |
| Capability evidence | Supported summary, deletion, merge, or window framework | A physically unsupported alternative is unavailable. |

One fact may support more than one calculation. Cardinality, for example, may
affect both an accuracy bound and a resource estimate. The consumer determines
its role; the word “evidence” does not mean “cost measurement.” Historical
observations also do not prove a permanent input-domain invariant unless the
named contract enforces that invariant for the plan's lifetime.

## Vocabulary across Planner and runtime

The input and output workflow uses different terms for different decisions.
They must not be collapsed into one generic “window,” “implementation,” or
“boundary” concept.

| Term | Owner | Meaning |
|---|---|---|
| Query window | Query semantics | The interval requested by the query, such as the five minutes in `data[5m]`. |
| Evaluation cadence or slide | Workload semantics | When the query is evaluated; it does not say how state is stored. |
| Summary window framework | ASAPPlanner | The abstract algorithm for organizing maintained summary state: tumbling, sliding, exponential histogram, or a registered extension. |
| Physical window layout | Downstream runtime | The concrete storage organization, such as full-window states, fixed panes, or hierarchical pane rollups. |
| Pane layout | Planner-runtime interface | Pane width and phase/origin needed to prove exact temporal coverage. A pane is a disjoint stored interval, not the query window itself. |
| Window-edge coverage | Planner-runtime interface | How partial intervals at a query window's edges are answered, for example by alignment or an exact residual. |
| Physical handoff | Physical costing/runtime | A network transfer or intermediate materialization. It is unrelated to a query-window edge. |
| Comparison scope | Costing | The common source, predicates, time selection, recurrence, and horizon over which raw and summary costs are comparable. |

These concepts may map to enums in Planner or a downstream repository, but an
enum is justified only when its variants express a live decision at that
owner's layer. For example, Planner's `SummaryWindowFramework` describes an
abstract choice it can compare; a backend physical-layout enum describes how
that selected choice is stored. A second enum that repeats the same decision
without adding an ownership or translation boundary should be removed or kept
internal.

Use **realization** for a candidate physical form and reserve
**implementation** for executable code. Use **schema resolution** for resolving
names and types, **window edge** for temporal coverage, **physical handoff** for
transfer or materialization, and **comparison scope** for cost comparability.
These names keep the workflow understandable while compatibility aliases remain
in code.

## Output contract

### Canonical output: `PlanSpace<Id>`

`PlanSpace` is the complete logical output of search. It contains:

- the workload roots after canonical common-subexpression sharing;
- one `MemoGroup` for every discovered target sub-DAG;
- every legal replacement candidate retained for that target;
- rejected candidates and their reasons; and
- prepared cross-group composition information used by selection.

The output is a **space of DAG choices**, not one executable DAG. A memo group
is a decision point for one canonical subexpression, not a standalone workload
plan. Choosing the first candidate independently in every group is not a valid
substitute for whole-plan selection because sharing and composition decisions
can change downstream uses.

`PlanSpace::cost_sorted` is a read-only ranked view:

```text
Vec<RankedGroup {
    target,
    consumer_count,
    candidates,
    costs, // index-aligned with candidates
}>
```

It is intended for inspection, explanation, or a downstream physical planner
that must retain alternatives. A listed candidate is logically available; its
presence alone does not prove that the deployment can execute it.

### Selected output: `GlobalSelection` and materialized DAGs

`PlanSpace::global_selection*` coordinates choices across memo groups.
`GlobalSelection::materialize(root)` returns the chosen `Rc<SummaryNode>`
Post-ASAP DAG for that root. A workload therefore produces one materialized
root per input query ID, with shared `Rc` nodes where the selection shares
state.

The materialized DAG records logical semantics, including summary family and
parameters, operations, schemas, windows, and result guarantees. It is not yet
an executable deployment plan. A downstream compiler must bind it to supported
physical operators and may preserve `KeepPreAsap` regions for exact execution.

### Lifecycle-aware selected output

`SummaryMaintenanceLifecyclePlan` adds the information needed to explain a
workload-aware maintenance decision:

- the materialized root;
- selected or rejected lifecycle alternatives for each summary state;
- horizon, evaluation rate, update rate, and expected reads;
- selected window implementation identity and accuracy guarantee, when known;
- summary and raw-recompute costs, when comparable; and
- whether raw recomputation was selected.

This remains a planner contract, not runtime configuration. The downstream
system compiles, deploys, and executes it.

## Supported workflows

### Workflow 1: inspect all logical alternatives

Use this workflow for tooling, explanations, or a downstream optimizer that
performs its own physical comparison.

```text
PlanningWorkload
  -> frontend lowering
  -> search_workload_with_targets
  -> PlanSpace
  -> cost_sorted (optional view)
```

Promise: all alternatives retained by the configured semantic, accuracy, and
evidence checks are visible.

Does not promise: one committed plan, complete physical feasibility, calibrated
deployment cost, or a lifecycle decision.

### Workflow 2: select a logical DAG

Use this workflow when the caller wants Planner to coordinate sharing and
composition choices but does not need a maintenance-versus-recompute decision.

```text
PlanSpace
  -> global_selection
  -> materialize each workload root
  -> selected Post-ASAP DAGs
```

Promise: choices are structurally compatible across the logical workload.

Does not promise: that the selected summary has a deployable physical
implementation or that maintaining it is cheaper than raw execution.

### Workflow 3: make a lifecycle-aware planning decision

This is the recommended workflow for a downstream system deciding whether to
deploy maintained summary state.

```text
PlanningWorkload + Pre-ASAP roots
  -> search_workload_with_targets
  -> global_selection_with_summary_maintenance_lifecycles
  -> materialize_with_summary_maintenance_lifecycles
  -> lifecycle-aware Post-ASAP plans
  -> downstream physical binding and deployment
```

Promise: the selection uses declared recurrence, data arrival, horizon,
capabilities, and comparable raw/summary costs. Missing required facts fail
closed instead of becoming optimistic zeroes.

Does not promise: placement, cluster-capacity feasibility, runtime readiness,
or execution. Those remain downstream responsibilities.

## Replanning

Replanning uses the same contract as initial planning. There is no separate
mutable Planner session whose hidden state changes the answer.

The caller should invoke the workflow again when any selection-relevant input
changes, including:

- query text, schema, or accuracy requirement;
- recurrence, horizon, data arrival, or distribution;
- evidence expiration or replacement;
- supported physical capabilities;
- cost calibration; or
- available materialized state.

```text
new workload snapshot + new evidence/capabilities
                    |
                    v
             run planning again
                    |
                    v
       new PlanSpace / selected contracts
                    |
                    v
 downstream compares, transitions, and activates
```

Planner produces the new logical decision. The downstream system owns diffing
old and new deployments, migration, activation ordering, and rollback. Evidence
from one planning snapshot must not be silently reused after its validity or
comparison scope changes.

## API guidance

For normal integrations:

- lower the complete workload through one frontend;
- call a whole-workload `search_workload*` function once;
- use the standard strategy factory rather than invoking individual strategies;
- preserve explicit accuracy targets and required evidence;
- use lifecycle-aware selection before claiming that a maintained summary is
  preferable to exact recomputation; and
- treat physical compilation and deployment as a downstream step.

The lower-level public traits and functions support research and deployment
extensions. They are not additional mandatory stages and should not be
presented as independent end-user workflows.

Public Rust visibility does not by itself make a type part of the recommended
integration surface. New public enums, variants, and extension points require
a concrete workflow that consumes them. API review should remove or internalize
duplicate concepts, unused variants, and compatibility types after their
consumers have migrated. This document names the intended external concepts;
the library reference records the current Rust entry points.

## Related documents

- [Planner pipeline](../concepts/planner-pipeline.md)
- [Pre-ASAP IR](../concepts/pre-asap-ir.md)
- [Post-ASAP IR](../concepts/post-asap-ir.md)
- [Planner/downstream boundary](planner-downstream-boundary.md)
- [Plan search internals](asap-aware-plan-search.md)
- [Public library reference](../../develop_docs/library-api.md)
