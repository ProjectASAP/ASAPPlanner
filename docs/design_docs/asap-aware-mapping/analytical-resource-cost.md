# Analytical resource cost model

## Purpose and boundaries

The analytical resource cost model compares legal physical plans using CPU
work, peak memory, and source/disk I/O. It replaces dimensionless plan-node
counts with estimates derived from operator complexity, cardinality, row
width, and concrete summary parameters.

The model does not decide semantic or accuracy legality. Candidate generation
and guarantee composition run first; costing ranks only the candidates that
survive. Missing evidence produces an unavailable estimate, never an assumed
zero or a structural-cost fallback.

This document distinguishes two implementation layers:

- the physical-DAG estimator, which can compose any DAG whose nodes have
  supported physical operators and complete `OperatorInputs`; and
- the planner bridge, which lowers a deliberately small set of replacement
  shapes into that estimator and participates in automatic candidate ranking.

Support in the first layer does not imply that the planner can yet lower and
rank the same shape. The current bridge boundary is stated explicitly below.

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

`AnalyticalInputs` and `OperatorInputs` are flattened arguments after this
evidence has been resolved. They are estimator adapters, not planner domain
objects. `dag_export --analytical-cost-json` is a development/reproducibility
adapter, not a replacement for `QueryWorkload` or `DataWorkload`.

The available canonical workload adapter is `AnalyticalCostModel::from_workload`. It
reads fresh source cardinality from `DataWorkload`, derives the finite
evaluation count from `QueryWorkloadEntry.recurrence` plus the planning
horizon, and combines those values with `PhysicalInputEvidence`. An unknown
recurrence, a zero horizon, no invocation inside the horizon, or stale source
cardinality makes the estimate unavailable. Query-shape constants such as
Top-K `k` are read from the lowered target rather than copied out of the
workload.

## Workload horizon and lifecycle

Every alternative must cover the same source data and query horizon. The
implemented analytical comparison is build-once, read-many:

```text
retained-summary builds = 1
query reads             = evaluation_count
updates after build     = 0
```

`evaluation_count` is derived from `QueryRecurrence` over a finite horizon.
It is not an independent workload axis. A repeated rate without a horizon
cannot produce a finite total cost.

The sketch alternative scans the selected source snapshot once and retains
state. The raw alternative recomputes from that snapshot for every query
read. Continuously ingesting data, rebuilds, deletions, expiration, and
retention duration belong to the summary-maintenance lifecycle model. They
must contribute update/build/delete work before a continuously maintained
plan is compared with raw execution.

## General DAG costing

Costing operates on the physical DAG, not on a list of logical operators.
Each costed node produces:

```text
NodeEstimate {
    output_rows,
    output_bytes,
    cpu_ops,
    retained_bytes,
    working_bytes,
    source_read_bytes,
}
```

The node's output statistics feed its parents. A parent never substitutes
the original source cardinality for an intermediate edge.

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

`estimate_physical_dag` implements these rules for `PhysicalDagNode` values.
Node IDs are physical identities: duplicate IDs, missing children, and cycles
are rejected. A child-before-parent schedule maintains remaining-consumer
counts, releases transient output after its last consumer, and keeps retained
state live. Consequently a shared scan is charged once per execution and a
fan-out's memory includes the outputs that really coexist.

Logical `output_bytes` feeds parent cardinality estimates; it is not an
allocation. Each physical node separately supplies `output_buffer_bytes` for
its live batch/edge buffer and `retained_bytes` for state that survives the
operator. A streaming scan therefore retains a batch, not the complete source.

Every node declares `ExecutionMultiplicity::Once` or `PerEvaluation`.
Build/maintenance nodes can therefore be charged once while query-side nodes
are multiplied by the horizon's evaluation count; retention does not silently
imply either execution frequency.

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
| Scan | `input_rows` | one input row/batch | `input_bytes` |
| Filter | `input_rows` | one output row/batch | `0` |
| Project or scalar pass-through | `input_rows` | one output row/batch | `0` |
| Hash aggregate | `input_rows` | `groups × (key + aggregate_value_bytes + hash metadata)` | `0` |
| Deduplicate | `input_rows` | keyed hash state | `0` |
| In-memory sort | `rows × ceil(log2(rows))` | `input_bytes` | `0` |
| Heap Top-K | `rows × ceil(log2(max(k, 2)))` | `min(k, rows) × row_bytes` | `0` |
| Hash join | `left_rows + right_rows + output_rows` | explicitly selected build-side bytes | `0` beyond children |
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

A retained sketch performs one build and serves later reads from state:

```text
cpu_ops = input_rows                         // build scan
        + input_rows × update_ops(params)
        + evaluation_count × physical_sketch_count × read_ops(params)

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
`physical_sketch_count = subpopulation_count`. A shared layout has the number
of physical structures described by that layout; the model must not infer it
from logical group count alone.

Summary merge, subtract, delete, and readout are separate physical operators.
Their CPU and memory use the concrete summary state size and number of input
states. A plan using one of these operations is unavailable until the
corresponding formula and required lifecycle evidence are present.

The summary-candidate bridge currently has formulas for sketch build and
readout only. It therefore rejects summary join, merge, subtract, and delete
explicitly. It never walks through one of those nodes and charges only its
children. Repeated `Rc<SummaryNode>` identities are deduplicated, and multiple
sketch states are sent through the physical-DAG estimator; the compact
single-summary adapter rejects them because it has no source-edge identities
with which to decide whether their reads are shared.

The compact planner bridge accepts only complete shapes it can currently lower
from its resolved inputs: an unfiltered source scan followed by one aggregate,
and the canonical count-grouped Top-K fusion over such a scan. It rejects a
filter, projection, join, window, nested aggregate, or predicate-bearing scan.
Although the standalone physical-DAG estimator can cost several of those
operators when given complete `OperatorInputs`, the planner does not yet have
an input path that lowers arbitrary candidate DAGs into it. Adding that lowering
and its statistics provider is future integration work. Final selection
excludes every unavailable replacement; when none remain, `chosen = None`
preserves the raw pre-ASAP target.

DDSketch is unavailable because occupied bins depend on value range and
distribution. The model does not invent a bin count. Algorithm/parameter
mismatches and arithmetic overflow also fail closed.

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

The current planner bridge executes this pipeline only for the compact shapes
listed above. Other supported physical operators are currently usable through
`estimate_physical_dag`, but are not automatically reached by replacement
selection.

The raw baseline and selected alternative use the same source snapshot,
horizon, and calibration:

```text
benefit       = baseline_cost - selected_cost
benefit_ratio = benefit / baseline_cost
```

Exported annotations contain resource totals, workload horizon, resolved
operator inputs, calibration coefficients, evidence/model versions, and the
baseline reference. This makes the scalar reproducible and identifies which
resource dimension drove a decision.

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
