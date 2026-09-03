# Analytical resource cost model

## Purpose and boundaries

The analytical resource cost model compares legal physical plans using CPU
work, peak memory, and source/disk I/O. It replaces dimensionless plan-node
counts with estimates derived from operator complexity, cardinality, row
width, concrete summary parameters, and the selected deployment lifecycle.

There are two planner adapters. `AnalyticalPlannerCostModel` compares complete
raw and replacement physical DAGs during automatic planner ranking.
`StreamingAnalyticalCostModel` supplies the lifecycle planner with a
raw recomputation baseline plus build, update, read, retention, and retirement
costs for `DataArrival::ContinuouslyIngesting`. The same module also exposes a
whole-selected-summary estimator for merge, subtract, delete, readout, and
join shapes.

The model does not decide semantic or accuracy legality. Candidate generation
and guarantee composition run first; costing ranks only the candidates that
survive. Missing evidence produces an unavailable estimate, never an assumed
zero or a structural-cost fallback.

This document distinguishes four implementation layers:

- the physical-DAG estimator, which can compose any DAG whose nodes have
  supported physical operators and complete `OperatorStatistics`; and
- query-DAG lowering, which recursively maps supported resolved `QueryExpr`
  operators to that physical representation;
- the deployment summary binder, which maps a selected `SummaryExpr` DAG to
  physical summary operators and snapshots their evidence; and
- the planner-ranking adapter, which compares the complete raw and replacement
  DAGs before making a candidate available to global selection.

Support in the estimator does not imply that a deployment has selected and
bound that physical algorithm. Unknown query lowering or summary binding makes
the entire alternative unavailable.
No layer may substitute a shape-specific shortcut or structural node count.

An estimate has physical dimensions:

```text
ResourceEstimate {
    cpu_ops,
    peak_memory_bytes,
    scan_bytes,
}
```

- `cpu_ops` is algorithmic work, not elapsed CPU time.
- `peak_memory_bytes` is simultaneously live retained and working state.
- `scan_bytes` is source/disk data read, not logical consumption of an
  already-materialized in-memory edge.

## Canonical workload and evidence sources

The cost model does not define a second workload schema. It consumes the
normalized workload, lowered query IR, and freshness-aware statistics:

| Cost concept | Canonical source |
|---|---|
| Source row cardinality | Fresh `DataWorkload.input_cardinality: Evidence<u64>`. |
| Data distribution | Fresh `DataWorkload.distribution: Evidence<DataDistribution>`. |
| Arrival and update rate | `DataWorkload.arrival`, `ingestion_volume`, and `ingestion_rate`. |
| Query demand | `QueryWorkloadEntry.recurrence`, normalized over an explicit horizon. |
| Event-time range | `QueryWorkloadEntry.time_selection`. |
| Accuracy and latency requirements | `QueryWorkloadEntry.requirements`. |
| Operator shape, grouping keys, and constants such as Top-K `k` | Lowered `QueryExpr` and `AggIntent`. |

Evidence is read through `Evidence<T>::value_at(planning_time)`. Stale,
future, or improperly time-bounded evidence remains unknown. Costing follows
the same freshness rule as accuracy and lifecycle planning.

The current workload schema does not yet contain every physical statistic.
The missing facts have explicit ownership:

| Physical statistic | Ownership |
|---|---|
| Source byte size and average row width | Fresh data/catalog evidence. |
| Distinct count for a grouping-key tuple | Per-key-set catalog or observed evidence. |
| Encoded key width | Schema/catalog evidence. |
| Filter selectivity and node output cardinality | Operator-statistics provider. |
| Join-side and join-output cardinality | Join-key/operator statistics. |
| Memory budget, spill I/O, cache behavior, and network bytes | Deployment/execution-profile evidence. |

`OperatorStatistics` contains the resolved statistics for one physical
operator. It is estimator evidence, not another planner workload object. The
lowering provider owns resolving it from the canonical workload, catalog, and
operator-statistics sources. Missing required evidence makes the entire plan
unavailable.

The incremental adapter is `StreamingSummaryInputs::from_workload`. Its public
entry point requires the
same `ComparisonScope` and `SourceCoverage` as the raw baseline; planning
time, horizon, recurrence, evaluation count, and arrival mode are not copied
into `StreamingSummaryInputs`. It additionally requires a fresh
`DataWorkload.ingestion_rate`. An unknown recurrence, a zero horizon, no invocation inside the
horizon, or stale evidence makes either estimate unavailable. Query-shape
constants such as Top-K `k` are read from the lowered target rather than
copied out of the workload.

## Workload horizon and lifecycle

Every alternative must cover the same source data and query horizon. The
implemented `DataArrival::AtRest` comparison is build-once, read-many:

```text
retained-summary builds = 1
query reads             = evaluation_count
updates after build     = 0
```

`evaluation_count` is derived from `QueryRecurrence` over a finite horizon.
It is not an independent workload axis. A repeated rate without a horizon
cannot produce a finite total cost.

An at-rest estimate must not be reused for `unknown`, `mixed`, or
`continuously_ingesting` data. The incremental adapter requires
`DataArrival::ContinuouslyIngesting`; it never passes that
workload through the at-rest formula. `Unknown` fails closed in both adapters.
`Mixed` also fails closed because `DataWorkload` does not currently separate
the at-rest backlog cardinality from the continuing stream cardinality. Using
one ambiguous value for both would double-count or omit work.

The at-rest sketch alternative scans the selected source snapshot once and
retains state. The raw alternative recomputes from that snapshot for every
query read. The incremental adapter charges the bootstrap scan, every arriving
update over the horizon, active and retained window state, query-side summary
operations, and update-side deletes. A pure streaming deployment may bootstrap
from `{ rows: 0, bytes: 0, source_scan_bytes: 0 }`; row and byte evidence may
not disagree. Zero bootstrap work is not a fabricated at-rest backlog.

The streaming raw baseline is rooted at the same planning-time rows, logical
bytes, source scan, and source-coverage identity as the summary bootstrap.
Each candidate comparison is explicitly bound by `(target, root)`; the
estimator never selects an arbitrary aggregation from a process-wide evidence
map. Raw rows, bytes, source scan, and ingestion rate belong to the target.
Nested and sibling aggregation inputs may be different intermediates, and
missing evidence in one candidate does not contaminate another candidate.
Binding validates the target QueryExpr's actual source and leaf predicates
against the scope, then publishes the context atomically.
Each evaluation adds rows and bytes that arrived since planning time, using
the recurrence's evaluation offsets. Multi-source raw comparison requires
per-source raw evidence and currently fails closed.

The streaming raw evidence stores per-evaluation bootstrap dimensions, then
binds them into one horizon-total physical DAG. Because those node statistics
already sum every evaluation, all reachable raw DAG nodes execute with `Once`
multiplicity; accepting `PerEvaluation` would multiply the horizon twice.

Periodic rebuild and implicit expiration-policy CPU remain unavailable. The
authoritative lifecycle types express direct versus incremental maintenance,
activation/retirement intervals, retention, and evaluation schedule, but no
rebuild cadence or expiration algorithm. The model does not introduce a
parallel enum or interpret retirement as expiration. Expiration work is
charged only when lowering has selected an explicit `SummaryDelete` node.

The incremental estimator accepts the existing
`SummaryMaintenanceLifecycleGuarantee`; it does not define another deployment
mode. `SummaryMaintenanceMode` and `EvaluationSchedule` must match the
authoritative lifecycle planner derivation for the declared `DataArrival`.
An inconsistent tuple fails closed rather than being reinterpreted.

### Comparable source and workload scope

A lower numerical cost is meaningful only when the two plans answer the same
request over the same physical data. `ComparisonScope` therefore reuses the
canonical workload and query terms rather than defining parallel strings:

| Scope field | Authoritative type and meaning |
|---|---|
| Arrival mode | `DataArrival`; complete-DAG comparison supports `AtRest` and `ContinuouslyIngesting`, while each adapter rejects modes outside its contract. |
| Planning instant and finite horizon | `TimestampMs` and `DurationMs`. |
| Invocation schedule | `QueryRecurrence`; the evaluation count is derived, not copied. |
| Event-time coverage | `TimeSelection`. |
| Logical sources | the existing query-IR `Source`, one per scan. |
| Filters | canonical bound `Predicate` values copied from the query IR. |
| Physical source contents | provider-owned `snapshot_id` per source. |

The snapshot identifier is the only new scope concept. It is necessary because
`Source` names a metric or table but neither the query IR nor workload schema
identifies a catalog version, object generation, or storage snapshot. Reusing a
query timestamp would be incorrect: query event time and storage version are
independent facts.

`ComparisonScope::from_workload` copies arrival, recurrence, and time selection
from `DataWorkload` and `QueryWorkloadEntry`; the catalog/lowering boundary adds
the source, snapshot identifier, and canonical predicates. Raw and candidate
scopes must match exactly in every field before their estimates are compared.
Unknown subsumption such as "this wider retained summary covers the requested
interval" is not guessed here; it requires a separate semantic coverage proof.
Missing sources, empty snapshot identifiers, invalid recurrence, or a zero
horizon fail closed.

Every reachable physical `Scan` carries one exact `SourceCoverage` copied from
this scope. That coverage includes the existing `Source`, its provider-owned
snapshot ID, and canonical predicates. A scan with no coverage, or coverage
not present in `ComparisonScope.sources`, makes the plan unavailable. Other
operators cannot declare source coverage. This prevents a DAG over source B
from being estimated under source A's comparison scope.

## General DAG costing

Costing operates on the physical DAG, not on a list of logical operators.
An `OperatorStatisticsProvider` resolves one `OperatorStatistics` value for
each reachable physical node:

```text
OperatorStatistics {
    source_scan_bytes,
    inputs: [EdgeStatistics { rows, bytes }, ...],
    output: EdgeStatistics { rows, bytes },
    group_count,
    key_bytes,
    aggregate_value_bytes,
    k,
    hash_join_build_side,
}
```

The provider owns provenance, freshness, and derivation. The estimator resolves
each reachable node once, so one estimate cannot mix values across a live
catalog refresh. A scan has one external source input; every other input is in
the same order as `PhysicalDagNode.children`. Every parent input must equal the
corresponding child's output in both rows and bytes. Missing node evidence,
invalid arity, inconsistent edge dimensions, or a parent/child conflict makes
the entire DAG unavailable. `{ rows: 0, bytes: 0 }` is a valid empty logical
edge, including after a filter, join, limit, or aggregate; non-empty edges need
positive logical bytes so width-dependent formulas do not invent a row width.
A parent therefore cannot silently substitute the original source cardinality
for an intermediate edge.

`EdgeStatistics.bytes` is decoded logical data carried on an edge.
`source_scan_bytes` is physical storage I/O and is charged only by `Scan`.
Compression, column pruning, or encoded storage can therefore make these
values different; neither is inferred from the other. Non-scan operators must
report zero source bytes in the current in-memory operator model.

### Composition rules

For a selected DAG:

1. Traverse in topological order.
2. Estimate each distinct physical node once.
3. Add CPU operations across nodes and across executions in the horizon.
4. Add source/disk reads only at nodes that actually read source or spilled
   data; an in-memory edge contributes zero source reads.
5. Count a shared node once even when several parents consume it.
6. Compute peak memory from liveness: add states that coexist, but do not add
   disjoint transient buffers merely because both appear somewhere in the
   DAG.
7. Retained summaries remain live across reads. Streaming buffers may be
   released after their last consumer.

`estimate_physical_dag` implements these rules for `PhysicalDagNode` values,
one `ComparisonScope`, and an `OperatorStatisticsProvider`. It is a
single-plan diagnostic API. Code that ranks a raw and candidate plan must use
`estimate_physical_dag_comparison`, which validates exact scope equality before
estimating either plan and returns both dimensional estimates together.
Node IDs are physical identities: duplicate IDs, missing children, and cycles
are rejected. A child-before-parent schedule maintains remaining-consumer
counts, releases transient output after its last consumer, and keeps retained
state live. Consequently a shared scan is charged once per execution and a
fan-out's memory includes the outputs that really coexist.

Logical edge `bytes` feeds parent cardinality estimates; it is not an
allocation. Each physical node separately supplies `output_buffer_bytes` for
its live batch/edge buffer and `retained_bytes` for state that survives the
operator. A streaming scan therefore retains a batch, not the complete source.

Every node declares `ExecutionMultiplicity::Once` or `PerEvaluation`.
Build/maintenance nodes can therefore be charged once while query-side nodes
are multiplied by the horizon's evaluation count; retention does not silently
imply either execution frequency.

The parent/child compatibility rules are:

| Parent | Child | Validity |
|---|---|---|
| `Once` | `Once` | valid |
| `Once` | `PerEvaluation` | invalid; a build-once result cannot depend on repeated executions |
| `PerEvaluation` | `PerEvaluation` | valid |
| `PerEvaluation` | `Once` | valid only when the child exposes retained state |

For a tree-shaped pipeline, peak memory is normally the maximum live pipeline
state, not the sum of every node's memory. At a fan-out, join, merge, or nested
summary boundary, multiple child states may coexist and must be combined.

A rewrite candidate is costed from the rewritten physical DAG. It does not
inherit the target's old node costs. A replacement candidate includes any
newly embedded child summaries, while an independently shared child is
deduplicated by physical identity.

## Physical operator formulas

Operator estimates are local: child CPU and I/O are excluded and composed by
the DAG rules above.

| Physical operator | CPU operations | Local memory | Source/disk reads |
|---|---:|---:|---:|
| Scan | `input_rows` | one decoded input row/batch | `source_scan_bytes` |
| Filter | `input_rows` | one output row/batch | `0` |
| Project or scalar pass-through | `input_rows` | one output row/batch | `0` |
| Hash aggregate | `input_rows` | `groups × (key + aggregate_value_bytes + hash metadata)` | `0` |
| Deduplicate | `input_rows` | keyed hash state | `0` |
| In-memory sort | `rows × ceil(log2(rows))` | `input_bytes` | `0` |
| Heap Top-K | `rows × ceil(log2(max(min(k, rows), 2)))` | `min(k, rows) × row_bytes` | `0` |
| Hash join | `left_rows + right_rows + output_rows` | selected build-side logical bytes plus 16 bytes of hash metadata per build row | `0` beyond children |
| Concat | `output_rows` | one output row/batch | `0` |
| Ordered window | `rows × ceil(log2(rows))` | live partition/input bytes | `0` |
| Limit | `output_rows` | one output row/batch | `0` |

These formulas name physical implementations. An external sort must add
spill writes and reads; a nested-loop join must not use the hash-join formula.
If the physical choice or its required statistics are unknown, the estimate
is unavailable.

In particular, hash aggregation requires the concrete accumulator width, and
hash join requires an explicit left/right build-side choice. The estimator
does not assume one 8-byte aggregate value or silently choose the smaller join
input.

## Summary operator formulas

A retained summary performs one build and serves later reads from state. The
following expression applies to any summary family; `update_ops`, `read_ops`,
and state size are supplied by that family's physical implementation:

```text
cpu_ops = input_rows                         // build scan
        + input_rows × update_ops(params)
        + evaluation_count × summary_state_instances_per_window × read_ops(params)

scan_bytes = source_scan_bytes for the build
```

Concrete accuracy-sized parameters determine state and work:

| Summary | Update operations per row | Read operations | State bytes per physical instance |
|---|---:|---:|---:|
| CMS | `depth` | `depth` | `width × depth × 8` |
| CountSketch | `depth` | `depth` | `width × depth × 8` |
| CMSWithHeap | `depth + ceil(log2(heap_size))` | `depth + heap_size` | `width × depth × 8 + heap_size × 16` |
| CountSketchWithHeap | same form as CMSWithHeap | same form as CMSWithHeap | `width × depth × 8 + heap_size × 16` |
| KLL | `1 + ceil(log2(k))` | `ceil(log2(k))` | `k × 8` |
| HLL | `1` | `2^precision` | `2^precision` |
| KMV | `ceil(log2(k))` | `k` | `k × 8` |
| Theta | `ceil(log2(k))` | `k` | `k × 8` |

For a per-subpopulation layout,
`summary_state_instances_per_window = subpopulation_count`. A shared layout
has the number of physical summary-state instances described by that layout;
the model must not infer it from logical group count alone. These instances
may be sketches, exact accumulators, samples, wavelet states, fitted models,
or another supported summary representation.

Summary merge, subtract, delete, and readout are separate physical operations.
The incremental estimator discovers them from the selected `SummaryExpr` DAG
and requires a per-instance `SummaryOperationCpuEvidence` value for every
operation actually present. It does not define a duplicate query-method enum.
Merge, subtract, and readout counts are multiplied by query evaluations.
Their execution multiplicity is evidence on that exact physical operator; it
is not borrowed from the first aggregation below it. A delete is additionally
bound to the exact state deployment whose active lifecycle bounds expiration.
Delete work uses an explicit expiration/retraction event rate and routing
fanout; it is not inferred from ingestion. Insert work consists of bootstrap
rows routed to `bootstrap_window_count` plus each arriving row routed to every
active window.

Summary build, merge, subtract, delete, and readout require their own physical
operators and resolved statistics. Until an operation has such a formula, a
complete candidate containing it is unavailable; costing only its modeled
children would undercount the plan.

`estimate_physical_dag` is independent of query shape. A caller supplies the
complete physical DAG and per-node evidence. Filters, projections, joins,
windows, nested aggregates, Top-K, and shared sub-DAGs therefore use the same
estimation path. The generic query lowerer recursively maps the supported raw
query operators into that representation.

`AnalyticalPlannerCostModel` is the final-selection adapter. For every
candidate it obtains one canonical `ComparisonScope`, recursively lowers the
actual raw target, and then lowers a logical rewrite or requests the fully
bound physical DAG for a `SummaryExpr` candidate. The deployment implements
`PlannerPhysicalPlanProvider`: query-node evidence is consumed atomically by
the generic query lowerer, while summary binding returns a complete
`PhysicalDag`, including embedded raw work, build/read operators, retained
state, execution multiplicity, and source coverage. The adapter calls
`estimate_physical_dag_comparison`; it never calls `DefaultCostModel` or a
structural-node-count fallback for final cost.

`SummaryAgg` is not a traversal leaf: its child is recursively costed. Every
`KeepPreAsap` child requires horizon preprocessing CPU and workspace evidence
for that exact `Rc` identity, including an apparently simple scan. This CPU
explicitly excludes summary insertion. Bootstrap/source I/O is owned only by
the enclosing aggregation evidence, so recursion cannot charge the same read
twice. Missing evidence makes the whole candidate unavailable. The scan source
and leaf predicates discovered in each aggregation child must equal the indexed
`SourceCoverage`; an index cannot relabel source B as source A. The retained
`KeepPreAsap` subtree is also a validated physical DAG, so unmodeled operators
cannot hide behind one opaque CPU total. Its scan coverage must come from the
same comparison scope. The current bootstrap evidence shape assigns one source
to each aggregation input; a multi-source input (including PromQL info
enrichment with an auxiliary metric) is unavailable until the provider supplies
per-source streaming evolution rather than guessing how the global ingestion
rate is divided.

For bootstrap rows `B`, arriving rows `U`, active windows `A`, query
evaluations `Q`, physical instances `P`, and unique `SummaryAgg` states `S`:

```text
insert calls   = sum_per_state(B * bootstrap_window_count + U * A)
merge calls    = Q * P for each physical SummaryMerge operator
subtract calls = Q * P for each physical SummarySubtract operator
delete calls   = expiration_rate * active_seconds * delete_routing_fanout
readout calls  = Q * P * SummaryEstimate nodes in the DAG
```

Here `P` is the provider-declared execution multiplicity of each physical
operator, not a structural node count. Bootstrap scans are de-duplicated only
when their provider-owned physical-read identities match. Sharing the same
logical `SourceCoverage` is insufficient because two builds may independently
read that source. Persistent state is summed. Every transient node declares
workspace excluding its output buffer, plus a separate output-buffer size. A
child-before-parent schedule retains a completed output until its last physical
consumer, counts parent workspace and output during execution, and then
releases dead child buffers. Reference counts preserve shared-node lifetimes
without charging an already-released child's execution workspace.

`B` comes from fresh `DataWorkload.input_cardinality`. For `Shared` and
`ContinuouslyMaintained`, `U` is the conservative ceiling of
`DataWorkload.ingestion_rate * horizon`; for `Prepared`, it uses only the
intersection of the activation/retirement interval with the comparison
horizon. `Q` comes from `QueryRecurrence` over that same finite horizon. Atomic
CPU values come from `SummaryOperationCpuEvidence`. They may be benchmark
measurements or algorithmic operation estimates, but their provenance and
units must be consistent within one comparison.

Persistent memory is:

```text
(active_window_count + retained_window_count)
  * summary_state_instances_per_window
  * unique_summary_state_count
  * summary_state_bytes_per_instance
```

Merge or subtract additionally needs one transient result state per physical
instance. Streaming input bytes are not reported as disk scan bytes; only the
optional bootstrap source scan is.

The lifecycle planner represents retention as a `CostRate` because it composes
all lifecycle work over seconds. Resource calibration, however, prices peak
capacity once, not byte-seconds. The adapter therefore divides the calibrated
retained-memory charge by the comparison horizon before returning the
lifecycle primitive. Integrating that rate over the same horizon produces
exactly one peak-memory charge and agrees with complete-DAG costing.

The incremental estimator supports
merge, subtract, delete, and readout when their operation evidence is present.
`SummaryJoin` additionally requires `SummaryJoinEvidence`: matched state pairs
per evaluation, CPU operations per matched pair, and peak join working memory.
Those physical facts cannot be inferred from the logical join key; absence or
invalid evidence makes the whole estimate unavailable.

`AnalyticalPlannerCostModel` is the final-selection adapter. For every
candidate it obtains one canonical `ComparisonScope`, recursively lowers the
actual raw target, and then lowers a logical rewrite or requests the fully
bound physical DAG for a `SummaryExpr` candidate. The deployment implements
`PlannerPhysicalPlanProvider`: query-node evidence is consumed atomically by
the generic query lowerer, while summary binding returns a complete
`PhysicalDag`, including embedded raw work, build/read operators, retained
state, execution multiplicity, and source coverage. The adapter calls
`estimate_physical_dag_comparison`; it never calls `DefaultCostModel` or a
structural-node-count fallback for final cost.

A candidate is exposed to global selection only when both complete DAGs are
valid and its calibrated cost is strictly below the raw baseline. Missing or
stale evidence, an unknown physical algorithm, invalid edges, incomplete
source coverage, or a candidate that is not cheaper yields `None`. When no
candidate remains, `chosen = None` preserves the raw pre-ASAP target.

Complete-plan costing also changes CSE selection order. Global selection sends
all share and recompute alternatives through `candidate_cost`; it does not use
the legacy structural CSE hook to discard one arm first. Once an arm is chosen,
the existing consumer-count propagation records whether that physical
alternative shares or recomputes. Within each returned physical DAG, stable
provider-owned physical IDs deduplicate shared scans, builds, and retained
states. Logical `Rc` identity is never substituted for physical identity.

DDSketch is unavailable because occupied bins depend on value range and
distribution. The model does not invent a bin count. Algorithm/parameter
mismatches and arithmetic overflow also fail closed.

### Relationship to the ASAPQuery formulations

The ASAPQuery
[configuration formulation](https://github.com/ProjectASAP/ASAPQuery/blob/8aa93f417ee662c188d65da5eb20ceefa01e5c12/.design_docs/sketch-config-optimization-formulation.md)
and [MIP formulation](https://github.com/ProjectASAP/ASAPQuery/blob/8aa93f417ee662c188d65da5eb20ceefa01e5c12/.design_docs/optimizer-mip-formulation.md)
at source revision `8aa93f4` are source material for physical streaming
multipliers, not an additional ASAPPlanner domain model. Their
shared principles used here are: ingestion cost scales with arrival rate;
overlapping active windows multiply insert work and state; retained windows
consume persistent memory; and merge/subtract/readout are charged at query
recurrence.

The documents disagree on important details. One limits subtract to
non-overlapping/tumbling prefix states and charges all retained windows as
steady-state memory; the other permits subtract over sliding configurations
and omits retained-but-unused sliding storage from continuous memory. ASAPPlanner
therefore does not encode those assumptions in a second closed window enum.
Instead, the physical-plan provider enumerates every semantically and
operationally feasible complete implementation. Its stable, provider-owned ID
may identify a tumbling layout, a sliding layout, a PromSketch exponential
histogram, or another window framework. Each alternative binds complete
per-node evidence, including bootstrap routing, active and retained window
counts, state size, operations, and source coverage.

The planner takes the Cartesian product of legal lifecycle combinations and
those physical alternatives and prices each combination over the same
`ComparisonScope`. Missing evidence makes that physical alternative
unavailable; it cannot win through an optimistic zero. The cheapest complete
combination is selected, and its provider-owned physical-plan ID is retained in
the lifecycle plan and DAG export. Thus window *legality and enumeration* stay
with the physical provider, while cost-based *selection* belongs to the
planner. `SummarySubtract` is charged only when it is an actual physical DAG
node, and every physically retained window in the chosen evidence is charged.

The global facility-location/MIP decisions from those documents—deploying one
configuration and assigning multiple atomic queries to it—remain outside this
candidate-local comparison. The open physical-alternative interface does not
introduce `x`/`y` assignment variables; a future workload-level optimizer may
use the same complete estimates as coefficients.

## Accuracy evidence remains separate from cost

Cost evidence cannot replace accuracy evidence. Examples include:

- approximate Top-K requires a lower bound for the kth selected item, an
  upper bound for excluded items, and their union-bounded failure probability;
- distribution-sensitive sizing or error propagation consumes fresh
  `DataWorkload.distribution` evidence;
- a shared sketch layout requires a proven composition bound.

The planner first rejects candidates without the necessary guarantee. Only
the surviving candidates reach cost ranking.

## Calibration

Resource dimensions become one scalar objective through a versioned
deployment calibration:

```text
cost = cpu_ops × cost_per_cpu_op
     + scan_bytes × cost_per_scan_byte
     + peak_memory_bytes × cost_per_retained_byte
```

Coefficients must be finite and non-negative, with at least one positive.
`CostUnits` is not implicitly money, latency, or cost per second. Coefficients
may be fitted from measurements or encode a deployment resource policy. The
calibration version is exported so results from different policies are not
treated as directly comparable.

Changing calibration may change the selected plan. A memory-constrained
deployment and an I/O-constrained deployment need not choose the same legal
candidate.

## Candidate selection and provenance

The intended end-to-end selection pipeline is:

1. enumerates semantically valid alternatives;
2. checks end-to-end accuracy and lifecycle legality;
3. derives fresh workload and operator statistics;
4. sizes physical summary parameters;
5. estimates the complete candidate DAG;
6. applies calibration and ranks candidates by ascending cost.

The query lowerer and physical estimator cover the supported raw-query shapes
listed above. `AnalyticalPlannerCostModel` executes this pipeline for every
candidate supplied to `PlanSpace::global_selection`. Logical rewrites are
lowered recursively. Summary candidates participate only after the deployment
has bound their complete `SummaryExpr` DAG; there is no optimistic generic
summary fallback. The streaming adapter connects raw recomputation and
primitive summary lifecycle costs to the existing global lifecycle-selection
hooks.
The lifecycle planner enumerates compatible lifecycle combinations for the
unique `SummaryAgg` deployments and invokes `complete_summary_candidate_cost`
for each combination before selecting the minimum. The hook receives explicit
node-to-guarantee bindings plus the horizon and expected reads. Each logical
occurrence is looked up by exact `Rc` identity, while every
evidence record also carries a provider-owned physical identity. Equal physical
identities deduplicate work and retained state only when their operator facts,
edge statistics, lifecycle guarantee, and physical child identities agree;
conflicts make the candidate unavailable. Thus heterogeneous states are costed
independently and genuinely shared deployments once. Merge, subtract, delete,
readout, and join participate in automatic
candidate ranking. Exhaustive whole-root scoring is capped at 4,096 lifecycle
combinations because an arbitrary whole-candidate hook cannot be soundly
pruned by primitive costs; a larger space is unavailable rather than consuming
exponential planner time. If the root needs unavailable operation evidence, the
hook returns unavailable. Global selection then excludes that summary and
materialization retains the raw expression. A missing raw estimate also forces
raw fallback, because no public selection/materialization path may publish an
uncompared summary. The planner never falls back to the partial `SummaryAgg`
sum.

Before applying the following arithmetic, callers validate exact equality of
the raw and selected alternative's `ComparisonScope`, and use the same
calibration:

```text
benefit       = baseline_cost - selected_cost
benefit_ratio = benefit / baseline_cost
```

An export of a cost decision includes resource totals, calibration
coefficients, evidence/model versions, the baseline reference, validated
comparison scope, and per-node statistics provenance. These facts reproduce
why two estimates were considered comparable.

## Worked patterns

The following are examples of the general DAG rules, not special definitions
of the model.

### Exact grouped aggregation

For an 8-byte aggregate value and 16 bytes of hash metadata:

```text
entry_bytes = group_key_bytes + 8 + 16
memory          = group_count × entry_bytes
scan CPU/read   = input_rows
aggregate CPU   = input_rows
```

Every raw evaluation rebuilds this state and rereads the source.

An exact quantile does not use that formula. Its current in-memory baseline
charges the source scan plus `input_rows × ceil(log2(input_rows))` sort work
per evaluation and retains `input_bytes` as a conservative value-buffer upper
bound. Exact cardinality is unavailable until distinct-count evidence for the
measured value exists; `group_count` describes GROUP BY tuples and must not be
substituted for it.

### Count-ranked Top-K

The logical form `TopK(Count GROUP BY key)` can be implemented by one global
CMSWithHeap keyed directly by the grouping column:

```text
Scan -> CMSWithHeap(key) -> TopK readout
```

It is not one CMS per key. With `width = 272`, `depth = 5`, and
`heap_size = 10`, state is:

```text
272 × 5 × 8 + 10 × 16 = 11,040 bytes
```

This fusion is one instance of costing the selected physical DAG rather than
costing each logical aggregate independently.

### Shared sub-DAG

If two queries consume the same retained summary, build CPU, source scan, and
retained state are counted once. Each readout contributes its own read CPU.
If the planner chooses independent recomputation instead, both executions are
counted.

### Join and sort

A hash-join alternative needs both input cardinalities, output cardinality,
row widths, and build-side choice. A downstream in-memory sort consumes the
join output statistics. If the sort exceeds memory and no spill model is
available, that candidate is unavailable rather than costed as in-memory.

## Unsupported and future dimensions

An estimate is unavailable when required evidence, a physical formula, or a
finite horizon is missing. Structural node counts are never substituted.

The current scalar includes CPU, retained memory, and source/disk reads. A
complete deployment model may additionally require:

- source and spill writes;
- network transfer;
- cache residency;
- parallelism and contention;
- allocator fragmentation;
- wall-clock critical-path latency;
- energy or monetary cost.

Those dimensions should extend the resource vector and calibration. They
must not be silently represented as zero. Consumers render unavailable costs
as `Not estimated`.
