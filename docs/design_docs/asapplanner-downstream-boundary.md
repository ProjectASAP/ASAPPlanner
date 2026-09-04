# ASAPPlanner and downstream application boundaries

## Purpose

ASAPPlanner is a general search and selection framework. It decides which
logical summaries, maintenance lifecycles, and planner-visible realization
strategies best satisfy a workload. Downstream systems implement, deploy, and
execute the selected contracts.

The boundary is iterative. A downstream provider can enumerate feasible
implementations and report their resource and accuracy evidence; ASAPPlanner
uses that evidence to compare candidates and returns the selected identity and
semantic contract. Measured behavior under a concrete data workload can thus
influence planning without putting executor code or deployment topology inside
ASAPPlanner.

## Three decision layers

| Layer | Owner | Examples |
|---|---|---|
| Logical semantics and lifecycle | ASAPPlanner | Query rewrite; summary family and parameters; grouping; accuracy; `Ephemeral`, `Prepared`, `Shared`, or `ContinuouslyMaintained`; `DirectBuild` or `Incremental`. |
| Planner-visible realization strategy | ASAPPlanner, using provider evidence | Tumbling windows, sliding windows/panes, PromSketch exponential-histogram windows, or another registered framework; KLL versus DDSketch; selection among complete feasible alternatives. |
| Concrete implementation and deployment | ASAPQuery-backend and its workload optimizer | Library and data-structure implementation, exact pane layout, placement, sharding, storage, transmission, materialization IDs, executor configuration, and workload-wide assignment. |

ASAPCollector and the ASAPQuery data plane execute the compiled downstream
plans. They validate capabilities and plan identities, maintain or read the
specified state, and report runtime observations. They do not silently choose
a different summary, lifecycle, or realization framework.

## Incremental-maintenance example

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

The same boundary applies when ASAPPlanner selects a summary algorithm. Planner
can choose KLL rather than DDSketch, while downstream chooses the concrete KLL
implementation and runtime configuration that satisfies the selected parameter
and accuracy contract. Empirical KLL error, update work, state size, and readout
work observed on a particular workload can be fed back as evidence for later
Planner comparisons.

Planner selection does not imply that ASAPPlanner contains the implementation.
Conversely, downstream implementation freedom does not permit changing the
selected algorithm's semantics or guarantees.

## Iterative planning protocol

1. ASAPPlanner enumerates semantically valid logical summaries, lifecycle
   alternatives, and registered realization strategies.
2. A physical-plan provider maps those candidates to executor-feasible complete
   alternatives. Unsupported candidates are omitted or explicitly rejected.
3. The provider binds a stable alternative identity and complete evidence:
   source coverage, input/output edges, operation counts, update and bootstrap
   fanout, retained state, CPU, memory, I/O, and accuracy facts.
4. ASAPPlanner rejects missing or incomparable evidence, evaluates every legal
   candidate over the same workload horizon, and selects the best complete
   alternative. It never substitutes structural node counts or optimistic
   zeroes.
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
problems. ASAPPlanner selects among summary and realization alternatives;
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

- Planner owns candidate semantics and final candidate selection.
- Downstream owns concrete implementation, compilation, placement, and
  execution.
- A selected realization framework is a contract, not executor code.
- Physical capabilities constrain the candidate space before ranking.
- Physical identity and complete evidence accompany each implementation.
- Missing statistics, unknown algorithms, stale evidence, and unsupported
  capabilities make a candidate unavailable.
- Shared logical nodes remain shared across the boundary; physical sharing
  additionally requires compatible filters, grouping, windows, parameters,
  lifecycle, and guarantees.
- Collector, backend, and query plans are projections of one compiled decision
  and cannot be optimized independently into inconsistent semantics.

## Related documents

- [Post-ASAP IR](post-asap-ir.md)
- [Physical plan integration](asap-aware-mapping/physical-plan-integration.md)
- [Analytical resource cost](asap-aware-mapping/analytical-resource-cost.md)
- [Workload demand and summary lifecycle](asap-aware-mapping/workload-demand-and-summary-lifecycle.md)
- [ASAPCollector physical compilation](https://github.com/ProjectASAP/ASAPCollector/blob/87684f4b61514382d8b087724694f93187bfc19c/docs/design_docs/control-plane/post-asap-physical-compilation.md)
- [ASAPQuery configuration formulation](https://github.com/ProjectASAP/ASAPQuery/blob/main/.design_docs/sketch-config-optimization-formulation.md)
- [ASAPQuery optimizer MIP formulation](https://github.com/ProjectASAP/ASAPQuery/blob/main/.design_docs/optimizer-mip-formulation.md)
