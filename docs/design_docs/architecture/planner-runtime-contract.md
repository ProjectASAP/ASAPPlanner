# ASAPPlanner planner-runtime contract

## Purpose

ASAPPlanner produces `PlanSpace`, a compact logical candidate space. Integrators
may select candidates downstream or ask Planner's helpers to select and assemble
DAGs. Summary-maintenance lifecycle decisions belong to Planner only when the
integration uses its lifecycle-aware workflow; physical deployment and execution
remain downstream. The [input/output/workflow design](input-output-workflow.md)
defines this boundary.

A downstream provider can report implementation alternatives and their cost and
accuracy evidence for a Planner-owned comparison. The resulting
`SummaryMaintenanceLifecyclePlan` contains a Post-ASAP DAG root and maintenance
decisions; it is not an executable plan. Repeated provider calls do not constitute
an implemented end-to-end replanning or deployment-transition protocol.

## Three decision layers

| Layer | Owner | Examples |
|---|---|---|
| Logical candidate semantics | ASAPPlanner | Query rewrite; summary family and parameters; grouping; accuracy guarantees when established. |
| Summary maintenance and realization selection | Planner helpers when delegated to Planner; otherwise downstream | `Ephemeral`, `Prepared`, `Shared`, `ContinuouslyMaintained`; `DirectBuild` or `Incremental`; window implementations compared using provider evidence. |
| Concrete implementation and deployment | ASAPQuery-backend and its workload optimizer | Library and data-structure implementation, exact pane layout, placement, sharding, storage, transmission, materialization IDs, executor configuration, and workload-wide assignment. |

ASAPCollector and the ASAPQuery data plane execute the compiled downstream
plans. They validate capabilities and plan identities, maintain or read the
specified state, and report runtime observations. They do not silently choose
a different summary, lifecycle, or realization framework.

## Incremental-maintenance example

When the integration delegates summary-maintenance decisions to Planner,
ASAPPlanner may decide that a logical summary should be incrementally
maintained: new data updates existing summary state. It may also select the
planner-visible window realization—such as tumbling, sliding/panes, or an
exponential histogram—because those alternatives have different accuracy,
CPU, memory, and I/O behavior.

ASAPQuery-backend then implements the selected framework. For example, after
Planner chooses a sliding-window realization, the backend chooses the concrete
pane representation, runtime operator implementation, placement, sharding,
watermark behavior, and materialization identifiers. ASAPCollector maintains
the compiled panes and summary state.

Thus `Incremental` describes the state-update lifecycle, while tumbling,
sliding, and exponential-histogram describe realization algorithms. They are
distinct axes, but both can participate in ASAPPlanner's candidate space. The
backend still owns how the selected algorithms are physically realized.

## Summary-algorithm analogy

The same contract applies when ASAPPlanner selects a summary algorithm. Planner
can choose KLL rather than DDSketch, while downstream chooses the concrete KLL
implementation and runtime configuration that satisfies the selected parameter
and accuracy contract. Empirical KLL error, update work, state size, and readout
work observed on a particular workload can be fed back as evidence for later
Planner comparisons.

Planner selection does not imply that ASAPPlanner contains the implementation.
Conversely, downstream implementation freedom does not permit changing the
selected algorithm's semantics or guarantees.

## Iterative planning protocol (future integration)

The sequence below is an intended integration design, not one shipped public
API or a required path for every caller. Current provider and lifecycle helpers
support a bounded planning decision; cross-run identity, migration, activation,
and rollback are not an end-to-end Planner protocol.

1. ASAPPlanner enumerates semantically valid logical summaries, lifecycle
   alternatives, and registered realization strategies.
2. A physical-plan provider maps those candidates to executor-feasible complete
   alternatives. Unsupported candidates are omitted or explicitly rejected.
3. The provider binds a stable alternative identity and complete evidence:
   source coverage, input/output edges, operation counts, update and bootstrap
   fanout, retained state, CPU, memory, I/O, and accuracy facts.
4. The summary-maintenance-lifecycle-aware workflow compares supported
   alternatives over the same workload horizon. Missing or incomparable costs
   do not establish that maintaining a summary beats raw recomputation;
   structural scores and optimistic zeroes are not substitutes.
5. ASAPPlanner outputs the selected Post-ASAP semantics, lifecycle guarantees,
   realization contract, and chosen provider identity.
6. ASAPQuery-backend compiles that result into consistent `CollectorPlan`,
   `BackendPlan`, and `QueryPlan` projections and performs deployment-level and
   workload-wide optimization.
7. ASAPCollector and the ASAPQuery data plane validate and execute those plan
   projections and return observations for future planning.

The provider may run steps 2–4 repeatedly. For example, it can submit several
implementations of the same sliding-window contract, or several parameterized
window frameworks, while keeping implementation-specific details downstream.

## Cost and accuracy evidence

Evidence belongs to a complete candidate and one comparison scope. Equal
source, snapshot, predicates, event-time selection, recurrence, and horizon are
required before raw and post-ASAP costs can be compared. Evidence from
different physical alternatives is never mixed node by node.

ASAPPlanner may consume analytically derived or empirically calibrated facts.
Downstream measurements can capture effects that an abstract model misses,
such as cache behavior, serialization overhead, compression, spill I/O, or
data-distribution-dependent sketch error. Provenance and version information
must accompany those facts so stale observations fail closed.

`StreamingPhysicalPlanAlternative` is the current integration point for a
complete provider-enumerated implementation. Its identity is returned with the
winning lifecycle combination. More structured planner-owned realization
contracts can refine the candidate space without moving executor
implementation into ASAPPlanner.

## Workload-wide optimization

Candidate comparison and workload-wide deployment optimization are different
problems. When requested, ASAPPlanner selects among summary and realization alternatives;
ASAPQuery-backend retains ownership of facility-location decisions: sharing a
deployed configuration across atomic queries, assigning queries to deployments,
choosing hosts, and satisfying cluster capacity.

The ASAPQuery configuration and MIP formulations can supply physical
alternatives and coefficients. Their general principles also inform Planner
costing: arrival rate scales ingestion work, overlapping active windows
multiply update work and live state, retained windows consume memory, and
merge/subtract/readout work scales with query recurrence. Disagreement between
formulations must become distinct explicit alternatives, not hidden assumptions
in one cost formula.

## Boundary invariants

- Planner owns candidate semantics; selection may be performed by its helpers
  or by the downstream integrator.
- Downstream owns concrete implementation, compilation, placement, and
  execution.
- A selected realization framework is a contract, not executor code.
- Physical capabilities and evidence constrain deployment choices, not every
  logical candidate's presence in `PlanSpace`.
- Complete physical alternatives need identity and comparable evidence for
  cost-based deployment decisions.
- Missing evidence remains unknown. It does not certify a guarantee or make
  every dependent logical candidate disappear; for example, an unproven
  DDSketch ratio remains inspectable but is not automatically selected.
- Shared logical nodes remain shared across the planner-runtime contract; physical sharing
  additionally requires compatible filters, grouping, windows, parameters,
  lifecycle, and guarantees.
- Collector, backend, and query plans are projections of one compiled decision
  and cannot be optimized independently into inconsistent semantics.

## Related documents

- [Post-ASAP IR](../concepts/post-asap-ir.md)
- [Physical plan integration](physical-plan-integration.md)
- [Analytical resource cost](../proposals/asap-aware-mapping/analytical-resource-cost.md)
- [Workload demand and summary lifecycle](../proposals/asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
- [ASAPCollector physical compilation](https://github.com/ProjectASAP/ASAPCollector/blob/87684f4b61514382d8b087724694f93187bfc19c/docs/design_docs/control-plane/post-asap-physical-compilation.md)
- [ASAPQuery configuration formulation](https://github.com/ProjectASAP/ASAPQuery/blob/main/.design_docs/sketch-config-optimization-formulation.md)
- [ASAPQuery optimizer MIP formulation](https://github.com/ProjectASAP/ASAPQuery/blob/main/.design_docs/optimizer-mip-formulation.md)
