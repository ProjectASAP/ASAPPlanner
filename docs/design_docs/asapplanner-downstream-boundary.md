# ASAPPlanner vs. downstream applications: responsibility boundary

## Purpose

ASAPPlanner chooses the logical computation that can answer a workload.
Downstream systems turn that logical decision into deployable runtime plans and
execute them. Keeping this boundary explicit prevents a logical summary choice
from accidentally depending on one window library, storage engine, network
topology, or Collector implementation.

A summary lifecycle answers **when a logical summary state exists and how it is
maintained**. A physical deployment answers **how and where that state is
implemented**.

## Ownership

| Owner | Decisions |
| --- | --- |
| ASAPPlanner | Logical query-time semantics; summary family, algorithm, parameters, grouping, accuracy, and composition; `Ephemeral`, `Prepared`, `Shared`, or `ContinuouslyMaintained` lifecycle; `DirectBuild` or `Incremental` maintenance mode; legal evaluation schedule and exact fallback. |
| ASAPQuery-backend physical compiler and workload optimizer | Concrete tumbling, sliding/pane, exponential-histogram, or other window implementation; window size, slide, pane layout, retention, watermark and lateness policy; collector/backend placement, sharding, storage, transmission, materialization identity, and runtime capability checks; workload-wide sharing and query-to-deployment assignments. |
| ASAPCollector | Validate and execute `CollectorPlan`: observe inputs, maintain the specified physical summary state, transmit raw data or summary state, and report the active plan identity and runtime evidence. It does not select a different logical summary or physical layout. |
| ASAPQuery data plane | Validate and execute `BackendPlan` and `QueryPlan`: ingest and store summary state, apply readouts and remaining operators, enforce readiness and freshness, and execute the explicit exact fallback. It does not independently re-plan the summary or deployment. |

Downstream capabilities constrain which Planner lifecycle candidates are legal,
but they do not move physical deployment choices into ASAPPlanner. For example,
if no target supports incremental updates, the capability input makes that
lifecycle unavailable. It does not tell Planner to choose a particular window
framework.

## Boundary example: incremental maintenance

- ASAPPlanner decides whether a logical summary should use incremental
  maintenance—that is, whether existing summary state should be updated as new
  data arrives.
- ASAPQuery-backend decides how to physically implement that incremental
  maintenance, for example with tumbling windows, sliding windows and panes,
  a PromSketch exponential histogram, or another runtime-supported structure.
- ASAPCollector executes the physical plan produced by the backend and
  maintains the specified windows and summary states.

The key distinction is that `Incremental` describes how the summary is
updated, while tumbling, sliding, and exponential-histogram windows describe
its physical layout. These decisions are orthogonal. Choosing incremental
maintenance does not select a window framework, and choosing a tumbling window
does not decide whether the summary is ephemeral, prepared, shared, or
continuously maintained.

## Planning and execution flow

1. ASAPPlanner consumes the query workload, data-workload facts, accuracy
   requirements, and downstream capability constraints. It constructs legal,
   deployment-independent Post-ASAP summary and lifecycle alternatives.
2. ASAPQuery-backend enumerates executor-feasible physical deployments for
   those logical alternatives. Physical identities remain in the backend.
3. The backend supplies complete cost evidence for an implementation when a
   Planner lifecycle or logical-summary decision needs that evidence. Planner
   checks logical legality and returns the corresponding complete estimate;
   this exchange may repeat for several implementations.
4. ASAPQuery-backend performs the workload-wide physical selection. It chooses
   windows and panes, retention, placement, sharding, storage, transmission,
   and query-to-deployment assignments without changing Planner semantics.
5. The selected deployment-independent Post-ASAP DAG records the logical
   summary, lifecycle, and exact fallback. It does not record a physical
   window-plan identity. The backend compiles one consistent bundle containing
   `CollectorPlan`, `BackendPlan`, and `QueryPlan` projections. Their summary
   family, parameters, grouping, guarantees, windows, and materialization
   identities must agree.
6. ASAPCollector and the ASAPQuery data plane validate and execute their plan
   projections. Unsupported or stale plans fail closed rather than being
   silently coerced.

The physical compiler may use deployment measurements and analytical Planner
estimates while searching. That does not transfer ownership of the physical
choice to Planner.

## Cost-evidence exchange

Some Planner decisions, especially lifecycle selection, depend on the resource
cost of an implementation. The dependency is expressed as evidence rather than
as a Planner-owned physical configuration:

1. ASAPQuery-backend enumerates one executor-feasible physical deployment and
   retains its physical identity.
2. It supplies `StreamingNodeEvidence` for that deployment, including bootstrap
   routing, active and retained state counts, state size, physical operations,
   edges, and source coverage.
3. ASAPPlanner evaluates legal lifecycle combinations over the supplied
   `ComparisonScope`. Missing evidence makes the candidate unavailable; it is
   never interpreted as zero cost.
4. The backend repeats the evaluation for other physical deployments and then
   performs workload-wide placement, sharing, facility-location, and
   query-assignment optimization using the complete estimates as coefficients.

ASAPPlanner neither enumerates physical deployments nor returns a physical-plan
identifier. Physical identities stay in the compiled downstream plan bundle.

## Relationship to the ASAPQuery optimization formulations

The ASAPQuery
[configuration formulation](https://github.com/ProjectASAP/ASAPQuery/blob/8aa93f417ee662c188d65da5eb20ceefa01e5c12/.design_docs/sketch-config-optimization-formulation.md)
and [MIP formulation](https://github.com/ProjectASAP/ASAPQuery/blob/8aa93f417ee662c188d65da5eb20ceefa01e5c12/.design_docs/optimizer-mip-formulation.md)
describe physical configurations and workload-wide deployment assignments.
Those responsibilities belong to ASAPQuery-backend under this architecture,
not to a second physical-window model inside ASAPPlanner.

ASAPPlanner may reuse general resource principles from those formulations:
ingestion work scales with arrival rate, overlapping active states multiply
update work, retained states consume memory, and merge, subtract, and readout
work scale with query recurrence. These principles price evidence; they do not
select or name the physical deployment.

## Invariants at the boundary

- Planner query-time windows describe logical coverage, not runtime panes.
- `Incremental` is a maintenance mode, not a physical window kind.
- The physical compiler must preserve the selected statistic, parameters,
  grouping, accuracy guarantee, source coverage, lifecycle semantics, and exact
  fallback.
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
