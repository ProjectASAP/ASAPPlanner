# Analytical resource cost model

## Purpose and boundaries

The analytical resource cost model version implemented here is explicitly for
`DataArrival::AtRest`. It compares legal physical plans using CPU work, peak
memory, and source/disk I/O. It replaces dimensionless plan-node counts with
estimates derived from operator complexity, cardinality, row width, and
concrete summary parameters.

The model does not decide semantic or accuracy legality. Candidate generation
and guarantee composition run first; costing ranks only the candidates that
survive. Missing evidence produces an unavailable estimate, never an assumed
zero or a structural-cost fallback.

This document distinguishes three implementation layers:

- the physical-DAG estimator, which can compose any DAG whose nodes have
  supported physical operators and complete `OperatorStatistics`; and
- query-DAG lowering, which recursively maps supported resolved `QueryExpr`
  operators to that physical representation; and
- replacement lowering and ranking, which must compare complete alternatives.

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
| Operator shape, grouping keys, and constants such as Top-K limit and offset | Lowered query and selected physical plan. |

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

### Operator vocabulary and source of truth

The complete logical-to-physical boundary is specified in
[Physical plan integration](physical-plan-integration.md). This section
summarizes the part required by the resource estimator.

`PhysicalOperator` is the source of truth for the cost model's operator
vocabulary. `OperatorStatistics` is paired one-to-one with that enum: each
supported physical algorithm has one evidence shape containing exactly the
facts its formula consumes. Exhaustive matches enforce that a newly added
physical operator must define its arity, statistics variant, validation, and
resource formula.

Neither logical IR is the statistics schema:

```text
pre-ASAP QueryExpr  ─┐
                     ├─ physical lowering ─> PhysicalDagNode/PhysicalOperator
post-ASAP SummaryExpr┘                              │
                                                    v
                                           OperatorStatistics
                                                    │
                                                    v
                                            ResourceEstimate
```

The pre-ASAP IR describes exact query semantics. The post-ASAP IR describes
logical summary semantics, selected summary families, and summary operations.
Neither identifies every physical algorithm, buffer, build side, or execution
layout. For example, one logical `SummaryAgg` may lower to a CMS build, an
exact accumulator build, or another supported summary implementation; a
logical `SummaryEstimate` lowers to the corresponding physical readout. Those
physical nodes need different formulas and evidence even though they originate
from the same logical variant.

The operator list in this version covers the physical query operators declared
by `PhysicalOperator`. It is not a claim that every `SummaryExpr` variant has
already been physically lowered. Summary build, join, merge, subtract, delete,
and readout become costable only after their lowering introduces explicit
physical operators and matching statistics variants. Until then, a candidate
containing such an unlowered operation is unavailable rather than partially
costed.

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
`continuously_ingesting` data. Callers fail closed instead of pretending that
incremental updates are a one-time snapshot build.

The sketch alternative scans the selected source snapshot once and retains
state. The raw alternative recomputes from that snapshot for every query
read. Continuously ingesting data, rebuilds, deletions, expiration, and
retention duration belong to the summary-maintenance lifecycle model. They
must contribute update/build/delete work before a continuously maintained
plan is compared with raw execution.

### Comparable source and workload scope

A lower numerical cost is meaningful only when the two plans answer the same
request over the same physical data. `ComparisonScope` therefore reuses the
canonical workload and query terms rather than defining parallel strings:

| Scope field | Authoritative type and meaning |
|---|---|
| Arrival mode | `DataArrival`; this model accepts only `AtRest`. |
| Planning instant and finite horizon | `TimestampMs` and `DurationMs`. |
| Invocation schedule | `QueryRecurrence`; the evaluation count is derived, not copied. |
| Event-time coverage | `TimeSelection`. |
| Logical sources | the existing query-IR `Source`, one per scan. |
| Source selection | canonical bound `Predicate` values for ordinary scans and symbolic `InfoMatcher` values for info-metric scans. |
| Physical source contents | provider-owned `source_snapshot_id` per source. |

The snapshot identifier is the only new scope concept. It is necessary because
`Source` names a metric or table but neither the query IR nor workload schema
identifies a catalog version, object generation, or storage snapshot. Reusing a
query timestamp would be incorrect: query event time and storage version are
independent facts.

`ComparisonScope::from_workload` copies arrival, recurrence, and time selection
from `DataWorkload` and `QueryWorkloadEntry`; the catalog/lowering boundary adds
the source, snapshot identifier, and canonical source selection. Raw and candidate
scopes must match exactly in every field before their estimates are compared.
Unknown subsumption such as "this wider retained summary covers the requested
interval" is not guessed here; it requires a separate semantic coverage proof.
An empty source set is valid only for a fully source-free logical DAG such as
`time()`, a number literal, or `vector(1)`. After lowering, the reachable Scan
coverage set must equal the scope source set: a Scan query with an empty scope,
or a source-free query with a non-empty scope, fails closed. Empty snapshot
identifiers, invalid recurrence, or a zero horizon also fail closed.

Every reachable physical `Scan` carries one exact `SourceCoverage` copied from
this scope. That coverage includes the existing `Source`, its provider-owned
snapshot ID, and canonical ordinary predicates or info-metric matchers. A scan with no coverage, or coverage
not present in `ComparisonScope.sources`, makes the plan unavailable. Other
operators cannot declare source coverage. This prevents a DAG over source B
from being estimated under source A's comparison scope.

## General DAG costing

Costing operates on the physical DAG, not on a list of logical operators.
An `OperatorStatisticsProvider` resolves one `OperatorStatistics` value for
each reachable physical node:

```text
EdgeStatistics { rows, bytes }

OperatorStatistics =
    Scan {
        edges: UnaryEdgeStatistics,
        source_read_bytes,
    }
  | Filter { edges: UnaryEdgeStatistics }
  | Project { edges: UnaryEdgeStatistics }
  | HashAggregate {
        edges: UnaryEdgeStatistics,
        group_count,
        key_bytes,
        accumulator_bytes_per_group,
    }
  | InMemoryComparisonSort {
        edges: UnaryEdgeStatistics,
        input_partitioning: PartitionStatistics,
    }
  | TopK { edges: UnaryEdgeStatistics }
  | HashJoin { edges: BinaryEdgeStatistics }
  | HashDeduplicate {
        edges: UnaryEdgeStatistics,
        distinct_key_count,
        key_bytes,
    }
  | Concat { inputs, output }
  | InMemoryAnalyticWindow {
        edges: UnaryEdgeStatistics,
        input_partitioning: PartitionStatistics,
    }
  | Limit { edges: UnaryEdgeStatistics }
  | PassThrough { edges: UnaryEdgeStatistics }
  | PromqlRange { edges, max_window_samples_per_series }
  | PromqlSubquery { edges, subquery_steps }
  | PromqlBinary { edges, matching_key_bytes }
  | PromqlRelabel { edges }
  | PromqlInfoEnrich { edges, matching_key_bytes }
  | PromqlSeriesSample { edges, group_count, key_bytes }
  | PromqlScalarToVector { edges }
  | PromqlVectorToScalar { edges }
  | PromqlScalarLeaf { output, promql_output }
  | PromqlPerSeries { edges, accumulator_bytes_per_series }
  | PromqlPresence { edges }
}
```

`UnaryEdgeStatistics` contains exactly one input and one output;
`BinaryEdgeStatistics` contains an ordered pair of inputs and one output.
`Concat` is the only variadic case. `EdgeStatistics` itself intentionally
remains operator-independent: an edge carries logical rows and bytes, and the
same edge is the output of one node and an input of every consumer. Making the
edge type depend on either endpoint would prevent direct consistency checks.

PromQL operators additionally attach a typed `PromqlEdgeStatistics` view to
the same physical edge:

```text
PromqlEdgeStatistics {
    series,
    evaluation_steps,
    value_kind: Scalar | Vector | RangeVector,
}
```

Rows/bytes still describe materialized logical data; PromQL metadata describes
its series and step shape. They are complementary, not alternative
cardinalities. Unary, binary, and variadic wrappers preserve the physical
arity, and parent input metadata must exactly match the corresponding child
output. Once a child carries PromQL metadata, a parent may not silently drop
it. Scalar leaves have zero inputs and emit exactly one scalar row per step.

The outer enum is operator-specific. A Filter cannot accidentally carry
group cardinality, a Top-K statistics record cannot carry a join build side,
and a non-scan record cannot carry source-read bytes. Serialized evidence is
internally tagged by operator and rejects unknown fields.

Physical configuration is not catalog evidence and therefore lives on
`PhysicalOperator`:

| Physical operator | Configuration owned by the plan |
|---|---|
| `Filter` | predicate operations per input row |
| `Project` | expression/copy operations per input row |
| `HashAggregate` | grouping-key and accumulator counts |
| `InMemoryComparisonSort` | ordering-key count and whether ordering is partitioned |
| `TopK` | output `limit`, `offset`, and ordering-key count; heap capacity is `limit + offset` |
| `Limit` | output `limit` and `offset` |
| `HashJoin` | left or right build side and equality-key count |
| `HashDeduplicate` | deduplication-key count |
| `InMemoryAnalyticWindow` | partition/order-key counts and window-function work per row |
| `PromqlRange` | selector range duration |
| `PromqlSubquery` | range and optional explicit resolution |
| `PromqlBinary` | operation class, operand modes, match cardinality, and vector/vector hash-build side |
| `PromqlRelabel` | expression work per sample |
| `PromqlInfoEnrich` | info-selector matcher work per info row |
| `PromqlSeriesSample` | `limitk`/`limit_ratio` choice and grouping-key count |
| `PromqlPerSeries` | primitive update work and accumulator count |
| `PromqlPresence` | absence versus per-series-presence mode and test work per input row |

This distinction removes the former flat optional `k` field. A Top-K bound is
part of the chosen algorithm, while input/output cardinality and width are
observed or estimated facts about that operator in this workload.

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

The statistics inputs and `PhysicalDagNode.children` therefore have
different arity only for a source leaf:

| Operator shape | Statistics inputs | DAG children |
|---|---:|---:|
| `Scan` | 1 external source edge | 0 |
| Unary operator | 1 | 1 |
| `HashJoin` | 2 | 2 |
| `Concat` | one per input | one per input |

The implementation matches every `PhysicalOperator` variant explicitly. A new
operator cannot silently inherit unary arity; its statistics-input and
DAG-child counts must both be defined.

`EdgeStatistics.bytes` is decoded logical data carried on an edge.
`Scan.source_read_bytes` is physical storage I/O and is charged only by
`Scan`.
Compression, column pruning, or encoded storage can therefore make these
values different; neither is inferred from the other. Other statistics
variants have no source-read field, so charging source I/O at a non-scan node
is not representable. A non-empty Scan must report positive source-read bytes;
zero cannot stand in for missing I/O evidence.

Hash-aggregate validation distinguishes logical grouping from the workload's
observed number of groups. An ungrouped aggregate has
`grouping_key_count = 0`, `key_bytes = 0`, and `group_count = 1`, including on
empty input because SQL scalar aggregation still emits one row. A grouped
aggregate has at least one grouping key and positive encoded key width, but it
may report `group_count = 0` when its input is empty. In both cases output rows
must equal `group_count`.

### Composition rules

For a selected DAG:

1. Traverse in topological order.
2. Estimate each distinct physical node once.
3. Add CPU operations across nodes and across executions in the horizon.
4. Add source/disk reads only at nodes that actually read source or spilled
   data; an in-memory edge contributes zero source reads.
5. Count a shared node once even when several parents consume it.
6. Compute peak memory from liveness. During one node's execution, all live
   child outputs, the operator's local workspace, and its new output buffer
   coexist. Do not add disjoint transient buffers merely because both appear
   somewhere in the DAG.
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

Each estimate independently requires the semantic set of source coverages on
its reachable Scan nodes to equal `ComparisonScope.sources`. Multiple physical
Scans may repeat one coverage, but no scope source may be omitted and no Scan
may add another coverage. This invariant is enforced by the estimator itself,
including for callers that construct a physical DAG without the query lowerer.

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

### Query-DAG lowering and statistics contract

`lower_query_physical_dag` recursively lowers a resolved `Rc<QueryExpr>` and
returns a `PhysicalDag` containing both its nodes and root ID. It consumes the
existing query and physical-operator enums; it does not introduce a parallel
logical operator vocabulary. For every occurrence, the lowerer sends a
`PhysicalNodeRequest` containing the logical node, selected existing
`PhysicalOperator`, occurrence and synthetic-role metadata, already-lowered
child physical IDs, and any source coverage to a
`PhysicalNodeEvidenceProvider`. The provider atomically returns its own stable
`physical_id`, the authoritative `OperatorStatistics`, and explicit
`output_buffer_bytes`; logical edge bytes are never substituted for an
allocation. Missing evidence makes the entire query unavailable. The returned
`PhysicalDag` snapshots this evidence so costing does not re-read a live
catalog after lowering.

Each lowered Scan is bound to exactly one `SourceCoverage` in the comparison
scope by the existing source and canonical predicate values. The bound value
therefore also supplies the provider-owned snapshot ID. Zero matches fail as
outside scope; multiple matching coverages fail as ambiguous rather than
choosing an arbitrary snapshot. When a predicate-bearing logical Scan expands
to Scan → Filter, the synthetic Scan has its own physical ID, statistics, and
buffer evidence and carries that exact coverage; the Filter has separate
evidence and no source coverage.
`ComparisonScope.sources` is an order-independent set of semantic coverages;
duplicates are invalid. After lowering, every reachable physical Scan must use
a member of that set and every member must be used by at least one Scan.
Multiple independent physical Scans may use the same coverage, while a
provider-declared shared Scan uses it once, so those physical alternatives can
still be compared under the same semantic scope.

The lowering validates every physical edge before costing:

- `(rows = 0, bytes = 0)` is a valid empty edge, while positive rows still
  require byte-width evidence and zero rows cannot carry non-zero bytes;
- a unary operator's `input_rows` and `input_bytes` equal its child's output;
- a Scan's external logical input edge equals its output edge, including the
  synthetic raw Scan created for a predicate-bearing logical Scan;
- a hash join's left and right inputs equal the corresponding child outputs;
- Concat and `UNION ALL` input/output totals equal the checked sum of all
  child outputs; and
- row-preserving, reducing, and bounded operators obey their cardinality
  invariants.

The supported mappings are:

| Existing `QueryExpr` shape | Physical DAG |
|---|---|
| Scan without predicates | Scan |
| Scan with pushed predicates | Scan → Filter |
| Filter | Filter |
| Project | Project |
| Reducing Count/Sum/Min/Max/Avg/StdDev/Variance/Group/CountValues without HAVING | HashAggregate |
| Dedup | HashDeduplicate |
| Equi-Join | HashJoin whose build side is selected from child output evidence |
| Concat or `UNION ALL` | Concat |
| Sort with at least one ordering key | in-memory Sort |
| global non-empty-key Sort followed by Limit | heap TopK, with `k = offset + n` from the query IR |
| partitioned Sort followed by Limit | Sort → Limit |
| RowNumber/Rank/DenseRank SQLWindowFunc with non-empty order_by | InMemoryAnalyticWindow |
| identity TimeShift only | PassThrough |
| range selector `[duration]` | PromqlRange |
| PromQL subquery `[range:resolution]` | PromqlSubquery |
| PromQL scalar/vector or vector/vector binary expression | PromqlBinary |
| `label_replace`/`label_join` | PromqlRelabel |
| `info(...)` | info-series Scan → PromqlInfoEnrich |
| `limitk`/`limit_ratio` | PromqlSeriesSample |
| `vector(scalar)` / `scalar(vector)` | PromqlScalarToVector / PromqlVectorToScalar |
| scalar literal, `time()`, or `pi()` | PromqlScalarLeaf |
| supported fixed-state per-series range reduction | PromqlPerSeries |
| absence/presence reduction | PromqlPresence |

HAVING, cross-series PromQL aggregation, and ordered/distribution-dependent
per-series intents such as exact quantile, cardinality, and Top-K aggregate
intents remain unavailable until they have an explicit physical algorithm.
Hash-join lowering
also uses the bound left and right output schemas to prove that every equality
compares one column from each side; same-side or out-of-range `ColumnId`s fail
closed.

An `Rc<QueryExpr>` address is not physical identity. Every logical occurrence
is independent unless the provider returns the same non-empty `physical_id`.
Repeated IDs deduplicate only when operator, children, coverage, statistics,
and buffer evidence are identical; conflicting reuse fails closed. This
generic lowering creates raw-query operators with
`ExecutionMultiplicity::PerEvaluation` and zero retained state. Buffer sizes
are always provider-owned physical evidence.

The DAG itself implements `OperatorStatisticsProvider` over its evidence
snapshot. That boundary verifies that each map key equals the evidence's
embedded physical identity and that every node's buffer equals the provider
snapshot before returning statistics, preventing the public node and evidence
views from silently drifting apart.

Cross/non-equi joins, `INTERSECT`, `EXCEPT`, distinct `UNION`, and unlisted
PromQL algorithms stay unavailable. They are not treated as free pass-through
work. In particular, generic `HashAggregate` cannot masquerade as a PromQL
cross-series aggregation: such a candidate needs a physical operator whose
per-step series grouping and memory semantics are explicit.

## Physical operator formulas

Operator estimates are local: child CPU and I/O are excluded and composed by
the DAG rules above.

| Physical operator | CPU operations | Local memory | Source/disk reads |
|---|---:|---:|---:|
| Scan | `input_rows` | one decoded input row/batch | `source_read_bytes` |
| Filter | `input_rows × predicate_operations_per_row` | one output row/batch | `0` |
| Project | `input_rows × expression_operations_per_row` | one output row/batch | `0` |
| Scalar pass-through | `input_rows` | one output row/batch | `0` |
| Hash aggregate | `input_rows × (grouping_key_count + accumulator_count)` | `groups × (key_bytes + accumulator_bytes_per_group + hash metadata)` | `0` |
| Hash deduplicate | `input_rows × key_count` | `distinct_key_count × (key_bytes + hash metadata)` | `0` |
| In-memory comparison sort | `sum(n_i × ceil(log2(n_i)) × ordering_key_count)` | largest partition bytes | `0` |
| Heap Top-K | `rows × ceil(log2(max(min(limit + offset, rows), 2))) × ordering_key_count` | `min(limit + offset, rows) × row_bytes` | `0` |
| Hash join | `(left_rows + right_rows) × equality_key_count + output_rows` | selected build-side logical bytes plus 16 bytes of hash metadata per build row | `0` beyond children |
| Concat | `output_rows` | one output row/batch | `0` |
| In-memory analytic window | per-partition ordering work plus per-row function work | largest partition bytes | `0` |
| Limit | `min(input_rows, limit + offset)` | one output row/batch | `0` |
| PromQL range | `input_rows` | `input_series × max_window_samples_per_series × sample_bytes` | `0` |
| PromQL subquery | `input_rows + output_rows` | materialized inner-step input bytes | `0` |
| PromQL vector binary | `left_rows + right_rows + output_rows` | vector/vector match hash state, or one output row for scalar/vector | `0` |
| PromQL relabel | `input_rows × expression_operations_per_row` | one output row/batch | `0` |
| PromQL info enrichment | input/output visits plus matcher work | info-side series hash state | `0` |
| PromQL series sample | `input_rows + input_series` | selected-series key state | `0` |
| PromQL scalar/vector bridge | `input_rows + output_rows` | one output row/batch | `0` |
| PromQL scalar leaf | `evaluation_steps` | one scalar row | `0` |
| PromQL fixed-state per-series operation | `input_rows × operations_per_row` | `input_series × accumulator_bytes_per_series` | `0` |
| PromQL presence | `input_rows × operations_per_row + output_rows` | one output row/batch | `0` |

These formulas name physical implementations. The enum uses names such as
`InMemoryComparisonSort`, `HashDeduplicate`, and `InMemoryAnalyticWindow` so a
new algorithm cannot silently inherit a formula merely because it has the
same logical purpose. An external sort must add spill writes and reads; a
nested-loop join must not use the hash-join formula.
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
        + evaluation_count × physical_summary_count × read_ops(params)

scan_bytes = source_read_bytes for the build
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
`physical_summary_count = subpopulation_count`. A shared layout has the number
of physical structures described by that layout; the model must not infer it
from logical group count alone.

Summary merge, subtract, delete, and readout are separate physical operators.
Their CPU and memory use the concrete summary state size and number of input
states. A plan using one of these operations is unavailable until the
corresponding formula and required lifecycle evidence are present.

Summary build, merge, subtract, delete, and readout require their own physical
operators and resolved statistics. Until an operation has such a formula, a
complete candidate containing it is unavailable; costing only its modeled
children would undercount the plan.

`estimate_physical_dag` is independent of query shape. A caller supplies the
complete physical DAG and per-node evidence. Filters, projections, joins,
windows, nested aggregates, Top-K, and shared sub-DAGs therefore use the same
estimation path. The generic query lowerer recursively maps the supported raw
query operators into that representation. Replacement lowering remains a
separate layer and must include all summary-maintenance work.

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

The query lowerer and physical estimator cover the supported raw-query shapes
listed above. Replacement evidence resolution, legality checks, and planner
integration remain separate layers; each preserves the complete-plan and
fail-closed requirements above.

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
