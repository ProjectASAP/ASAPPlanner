# ASAPPlanner vs. downstream applications: responsibility boundary

## Purpose

ASAPPlanner searches over abstract primitives that can answer a workload.
Downstream systems enumerate concrete realizations, supply their measured or
analytical costs back to Planner, turn the selected abstract plan into
deployable runtime plans, and execute them. Keeping this boundary explicit lets
Planner discover and compare new primitives without depending on one library,
storage engine, network topology, or Collector implementation.

A summary lifecycle answers **when a summary state exists and how it is
maintained**. A window framework answers **which abstract time-organization
primitive is used**. A physical deployment answers **which concrete
implementation realizes those choices, and where it runs**.

## Ownership

| Owner | Decisions |
| --- | --- |
| ASAPPlanner | Logical query-time semantics; summary family, algorithm, parameters, grouping, accuracy, and composition; abstract window framework such as tumbling, sliding, or exponential histogram; `Ephemeral`, `Prepared`, `Shared`, or `ContinuouslyMaintained` lifecycle; `DirectBuild` or `Incremental` maintenance mode; legal evaluation schedule and exact fallback. |
| ASAPQuery-backend physical compiler and workload optimizer | Enumerate concrete implementations for every Planner candidate; report implementation cost and feasibility under the `DataWorkload`; choose the runtime library and configuration that realizes the selected sketch and window primitives; derive panes and retention layout; decide collector/backend placement, sharding, storage, transmission, materialization identity, and workload-wide query-to-deployment assignments. |
| ASAPCollector | Validate and execute `CollectorPlan`: observe inputs, maintain the specified physical summary state, transmit raw data or summary state, and report the active plan identity and runtime evidence. It does not select a different logical summary or physical layout. |
| ASAPQuery data plane | Validate and execute `BackendPlan` and `QueryPlan`: ingest and store summary state, apply readouts and remaining operators, enforce readiness and freshness, and execute the explicit exact fallback. It does not independently re-plan the summary or deployment. |

Downstream capabilities and costs constrain which Planner candidates are legal
and useful, but they do not move implementation identities or placement into
ASAPPlanner. For example, if no target can realize incremental updates for an
exponential histogram, that candidate is unavailable. If several runtimes can
realize it, their performance under the current `DataWorkload` is summarized as
cost evidence rather than encoded as Planner IR.

## Boundary example: incremental maintenance

- ASAPPlanner decides whether a logical summary should use incremental
  maintenance—that is, whether existing summary state should be updated as new
  data arrives.
- ASAPPlanner also decides which abstract window framework should organize that
  state, for example tumbling, sliding, or exponential histogram.
- ASAPQuery-backend decides which concrete library and configuration implements
  the selected framework, how panes and retention are laid out, and where the
  state runs. PromSketch may be one implementation of the exponential-histogram
  primitive.
- ASAPCollector executes the physical plan produced by the backend and
  maintains the specified windows and summary states.

The key distinction is that `Incremental` describes how the summary is updated,
while tumbling, sliding, and exponential histogram name abstract window
primitives. Those two Planner decisions are orthogonal. The concrete data
structure, executable parameters, placement, and deployment identity remain
downstream decisions.

## General primitive model

`SummaryWindowFramework` contains the built-in `Tumbling`, `Sliding`, and
`ExponentialHistogram` primitives plus a named `Extension`. The extension case
is a Planner candidate, not an escape hatch for an opaque deployment ID: its
semantics and cost-evidence contract must be registered before it can be
selected. This lets Planner expose and compare a newly discovered window
primitive in the same way that it can compare a newly supported sketch
algorithm.

A window candidate assigns a framework independently to every materialized
summary state in the DAG. This permits, for example, one shared state to use an
exponential histogram while another uses tumbling windows. Missing, duplicate,
or incomplete per-state assignments fail closed. A downstream provider must
collapse multiple concrete implementations of the same abstract assignment to
one feasible cost-evidence candidate; runtime implementation IDs never become
Planner semantics.

## Planning and execution flow

1. ASAPPlanner consumes the query workload, data-workload facts, and accuracy
   requirements. It constructs a deployment-independent candidate space over
   summaries, window frameworks, and lifecycle alternatives.
2. ASAPQuery-backend enumerates executor-feasible implementations for every
   Planner candidate. Concrete implementation identities remain in the backend.
3. The backend supplies complete cost evidence for an implementation when a
   Planner lifecycle or logical-summary decision needs that evidence. Planner
   checks logical legality and returns the corresponding complete estimate;
   this exchange may repeat for several implementations.
4. ASAPPlanner uses the returned evidence to select abstract summary, window,
   and lifecycle primitives. The backend performs the remaining workload-wide
   physical selection: concrete implementation and configuration, panes,
   retention layout, placement, sharding, storage, transmission, and
   query-to-deployment assignments.
5. The selected deployment-independent Post-ASAP DAG records the logical
   summary, window framework, lifecycle, and exact fallback. It does not record
   a concrete implementation or deployment identity. The backend compiles one
   consistent bundle containing
   `CollectorPlan`, `BackendPlan`, and `QueryPlan` projections. Their summary
   family, parameters, grouping, guarantees, windows, and materialization
   identities must agree.
6. ASAPCollector and the ASAPQuery data plane validate and execute their plan
   projections. Unsupported or stale plans fail closed rather than being
   silently coerced.

The feedback loop does not transfer concrete implementation ownership to
Planner, just as Planner may choose KLL while downstream compilation chooses a
specific KLL implementation.

## Cost-evidence exchange

Planner decisions such as KLL versus DDSketch, tumbling versus sliding, and
ephemeral versus incremental maintenance depend on how available
implementations perform for a specific data workload. That dependency is
expressed as evidence rather than as a Planner-owned implementation:

1. ASAPQuery-backend enumerates an executor-feasible implementation for one
   Planner candidate and retains its concrete identity.
2. It supplies `StreamingNodeEvidence` for that implementation, including bootstrap
   routing, active and retained state counts, state size, physical operations,
   edges, and source coverage.
3. ASAPPlanner evaluates legal lifecycle combinations over the supplied
   `ComparisonScope`. Missing evidence makes the candidate unavailable; it is
   never interpreted as zero cost.
4. The backend repeats this for the implementations of all Planner candidates.
   Planner ranks the abstract candidates; the backend retains the realization
   corresponding to the winning evidence and performs workload-wide placement,
   sharing, facility-location, and query-assignment optimization.

ASAPPlanner returns the selected `SummaryWindowFramework`, but not a concrete
implementation or physical-plan identifier. Physical identities stay in the
compiled downstream plan bundle.

## Relationship to the ASAPQuery optimization formulations

The ASAPQuery
[configuration formulation](https://github.com/ProjectASAP/ASAPQuery/blob/8aa93f417ee662c188d65da5eb20ceefa01e5c12/.design_docs/sketch-config-optimization-formulation.md)
and [MIP formulation](https://github.com/ProjectASAP/ASAPQuery/blob/8aa93f417ee662c188d65da5eb20ceefa01e5c12/.design_docs/optimizer-mip-formulation.md)
describe both abstract configuration choices and workload-wide deployment
assignments. Under this architecture, abstract primitives become Planner
candidates, while concrete realizations and deployment assignments belong to
ASAPQuery-backend.

ASAPPlanner may reuse general resource principles from those formulations:
ingestion work scales with arrival rate, overlapping active states multiply
update work, retained states consume memory, and merge, subtract, and readout
work scale with query recurrence. These principles price evidence; they do not
name a concrete implementation or physical deployment.

## Invariants at the boundary

- Planner query-time windows describe logical coverage. A selected Planner
  window framework describes an abstract realization strategy, not runtime
  panes or an implementation identity.
- `Incremental` is a maintenance mode, not a physical window kind.
- The physical compiler must preserve the selected statistic, parameters,
  grouping, window framework, accuracy guarantee, source coverage, lifecycle
  semantics, and exact fallback.
- Shared logical nodes remain shared across the compilation boundary. Physical
  sharing requires compatible filters, grouping, windows, parameters, and
  guarantees.
- Every query route refers to a declared materialization, and every
  Collector-produced materialization has a compatible backend declaration.
- Missing statistics, unsupported capabilities, unknown variants, and stale
  plan generations fail closed.
- Collector and data-plane plans are projections of one compiled decision; they
  are never optimized independently.

## Related documents

- [Post-ASAP IR](post-asap-ir.md)
- [Query workloads, data workloads, and summary lifecycle maintenance](asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
- [Analytical resource cost](asap-aware-mapping/analytical-resource-cost.md)
- [ASAPCollector physical compilation](https://github.com/ProjectASAP/ASAPCollector/blob/87684f4b61514382d8b087724694f93187bfc19c/docs/design_docs/control-plane/post-asap-physical-compilation.md)
- [ASAPQuery-backend physical compiler guide](https://github.com/ProjectASAP/ASAPQuery-backend/blob/main/docs/developer_docs/control-plane/physical-compiler.md)
