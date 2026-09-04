# Physical plan integration

## Purpose

This document defines the boundary between ASAPPlanner's logical plans,
physical lowering, statistics resolution, and analytical resource estimation.
It answers which representation is authoritative at each stage and prevents
the cost model from being coupled directly to either logical IR.

The integration pipeline is:

```text
pre-ASAP QueryExpr  ─┐
                     ├─ physical lowering ─> PhysicalOperator DAG
post-ASAP SummaryExpr┘                              │
                                                    v
                                           OperatorStatistics
                                                    │
                                                    v
                                            ResourceEstimate
```

The arrows are boundaries, not casts. A logical node does not necessarily
become one physical node, and a physical operator is not merely a renamed
logical variant.

## Sources of truth

Each representation is authoritative for a different concern:

| Representation | Authoritative concern |
|---|---|
| `QueryExpr` | Original exact query semantics: sources, predicates, relational and PromQL operations, and output shape. |
| `SummaryExpr` | Logical summary semantics: selected family, grouping strategy, summary composition, and summary readout. |
| `PhysicalOperator` DAG | Selected executable algorithms, their configuration, physical identity, edges, and execution multiplicity. |
| `OperatorStatistics` | Workload-dependent evidence required by each selected physical operator's resource formula. |
| `ResourceEstimate` | Estimated CPU operations, peak live memory, and physical source/disk reads over one comparison scope. |

`PhysicalOperator` is therefore the source of truth for the operator vocabulary
consumed by analytical costing. `OperatorStatistics` corresponds one-to-one
with that vocabulary. It must not independently invent operator kinds or copy
all variants from either logical IR.

The canonical physical-plan types should live at a neutral boundary shared by
lowering, costing, explanation, and downstream compilation. Their conceptual
ownership is not the cost formula implementation, even if an intermediate
implementation keeps the Rust type in the cost module while the interface is
being established.

## Why logical and physical variants are not one-to-one

One logical operation may choose between algorithms or expand into a physical
sub-DAG. Conversely, one physical operator may implement nodes originating
from either logical IR.

Examples include:

- `Sort` followed by `Limit` may lower to an in-memory comparison sort plus a
  limit, or to one heap Top-K operator configured by limit and offset.
- `Join` may lower to a hash join with an explicit build side or to another
  supported join algorithm.
- `SummaryAgg` may lower to an exact accumulator build, CMS build, KLL build,
  or another physical summary algorithm selected by the candidate.
- `SummaryEstimate` must lower to a readout operator compatible with the
  concrete summary state it consumes.
- shared logical sub-DAGs become shared physical nodes only when they refer to
  the same physical identity and compatible evidence.

For this reason, aligning `OperatorStatistics` directly with `QueryExpr` would
lose post-ASAP summary implementations, while aligning it directly with
`SummaryExpr` would lose raw query operators and physical algorithm choices.

## Lowering obligations

Physical lowering is complete only when it recursively lowers the entire
selected candidate DAG. It must:

1. preserve the semantics and source coverage of the logical candidate;
2. select an explicit physical algorithm for every logical operation;
3. carry algorithm configuration on the physical operator rather than in a
   generic statistics record;
4. connect every physical edge and preserve child order for non-commutative
   operators;
5. assign stable physical identities so shared sub-DAGs can be counted once;
6. assign execution multiplicity and retained/transient state ownership;
7. reject the complete candidate if any logical operation has no supported
   physical lowering.

Keeping an unsupported logical node outside the physical DAG and costing only
its modeled descendants is invalid because it undercounts the candidate.

### Pre-ASAP lowering

`KeepPreAsap` recursively lowers its contained `QueryExpr`. Typical physical
operators include scans, filters, projections, hash aggregates, joins,
ordering, bounded Top-K, limits, and PromQL-specific operators. The selected
physical algorithm, rather than the logical spelling, determines the formula.

### Post-ASAP lowering

Every `SummaryExpr` operation also needs explicit physical realization:

| Logical summary operation | Required physical realization |
|---|---|
| `SummaryAgg` | concrete summary or exact-state build/update operator, including family parameters and grouping layout |
| `SummaryJoin` | concrete summary-join algorithm and state layout |
| `SummaryMerge` | merge operator over compatible concrete summary states |
| `SummarySubtract` | subtract operator supported by the selected state representation |
| `SummaryDelete` | physical deletion/update operator supported by the selected representation |
| `SummaryEstimate` | family- and query-specific readout operator |
| `KeepPreAsap` | recursive lowering of the contained `QueryExpr` |

This table is a completeness requirement, not a claim that every realization
already exists. Until lowering introduces an explicit physical operator,
statistics contract, validation rule, and resource formula for an operation,
a candidate containing it is unavailable.

Lifecycle choice affects the physical DAG but does not replace it. Ephemeral,
prepared, shared, and continuously maintained alternatives determine when
build, update, readout, merge, subtract, or delete nodes execute. The physical
operators still determine how each execution consumes CPU, memory, and I/O.

## Statistics contract

Statistics describe how a selected physical algorithm behaves for a specific
workload and data snapshot. They do not select the algorithm.

Every physical node supplies:

- logical input and output edge cardinality and decoded byte size;
- operator-specific distribution or state facts required by its formula;
- physical source-read bytes only when the operator actually reads storage;
- provenance and freshness sufficient to reproduce the estimate.

Plan-owned configuration and observed statistics remain separate. For
example, a Top-K operator owns its limit and offset, while its statistics
describe input and output rows and bytes. A hash join owns its build-side
choice, while its statistics describe both input edges and its output edge.

The statistics enum is structured by `PhysicalOperator`. This prevents a
Filter from carrying group cardinality, a Top-K record from carrying join
facts, or a non-scan record from charging source bytes. A new physical variant
requires a corresponding statistics variant and exhaustive integration into:

- statistics and DAG arity validation;
- parent/child edge consistency validation;
- operator semantic validation;
- CPU, memory, and I/O formulas;
- lowering and coverage tests.

## Physical operator catalog

This catalog defines every physical node currently accepted by the analytical
estimator. SQL examples describe the logical shape; the physical node is the
algorithm selected when lowering that shape. A query outside the stated shape
is unavailable until another physical operator is defined.

### `Scan`

Reads a physical source snapshot and emits decoded logical rows. For example,
`SELECT * FROM metrics` lowers its source leaf to `Scan`. Its evidence contains
the external/output edge and `source_read_bytes`. A non-empty scan must report
positive physical read bytes. CPU is one decode/visit per input row; memory is
one decoded row or batch; I/O is `source_read_bytes`.

### `Filter { predicate_operations_per_row }`

Evaluates a scalar predicate and retains matching rows. For example,
`SELECT * FROM metrics WHERE latency > 100 AND status = 500` lowers to
`Scan -> Filter`. The configuration counts the comparison/boolean operations
in the predicate; the evidence provides input and filtered output edges. CPU
is `input_rows * predicate_operations_per_row`; memory is one output row or
batch; it adds no source I/O.

### `Project { expression_operations_per_row }`

Computes and copies a SELECT list without changing row cardinality. For
example, `SELECT latency * 1000 AS latency_us, service FROM metrics` lowers to
`Scan -> Project`. The configuration counts expression and output-copy work;
the evidence supplies the changed logical row width. CPU is
`input_rows * expression_operations_per_row`; memory is one output row or
batch; it adds no source I/O.

### `HashAggregate { grouping_key_count, accumulator_count }`

Builds hash-group state and updates one or more accumulators. For example,
`SELECT service, COUNT(*), SUM(bytes) FROM metrics GROUP BY service` uses one
grouping key and two accumulators. Evidence supplies `group_count`, encoded
key bytes, and total accumulator bytes per group. CPU is
`input_rows * (grouping_key_count + accumulator_count)`; memory is
`group_count * (key_bytes + accumulator_bytes_per_group + hash metadata)`.
An ungrouped aggregate has zero keys, zero key bytes, and exactly one output
group even for empty input. A grouped aggregate may have zero groups when its
input is empty.

### `InMemoryComparisonSort { ordering_key_count, partitioned }`

Comparison-sorts rows without spilling. A global example is
`SELECT * FROM metrics ORDER BY latency`; a partitioned logical shape is
`ORDER BY latency` within each region. Evidence lists the observed input edge
for every independently sorted partition. CPU is
`sum(n_i * ceil(log2(n_i)) * ordering_key_count)` and peak local memory is the
largest partition bytes. A global sort must provide exactly one partition.
If a provider cannot prove an in-memory implementation or its partition
distribution, the candidate is unavailable rather than silently using a
global-sort estimate.

### `TopK { limit, offset, ordering_key_count }`

Maintains a bounded comparison heap for a global `ORDER BY ... LIMIT/OFFSET`.
For example, `SELECT service, count FROM counts ORDER BY count DESC LIMIT 10`
uses a heap of at most ten rows. CPU is
`input_rows * ceil(log2(min(limit + offset, input_rows))) * ordering_key_count`;
memory is the bounded heap rows times logical row width. Partitioned Top-K is
not this operator and requires its own supported physical realization.

### `HashJoin { build_side, equality_key_count }`

Builds a hash table on the selected side and probes it with the other side.
For example, `SELECT * FROM requests r JOIN services s ON r.service_id = s.id`
uses one equality key. Evidence supplies ordered left/right edges and the join
output. CPU charges equality-key hashing/probing for both inputs plus emitted
output rows; memory is the selected build-side bytes plus hash metadata. A
cross join, non-equality predicate, or unknown join algorithm is unavailable.

### `HashDeduplicate { key_count }`

Retains one hash-table entry per distinct key. For example,
`SELECT DISTINCT service, region FROM metrics` has two deduplication keys.
Evidence supplies distinct-key count and encoded key bytes. CPU is
`input_rows * key_count`; memory is
`distinct_key_count * (key_bytes + hash metadata)`. Empty input legitimately
has zero distinct keys.

### `Concat`

Concatenates one or more union-compatible inputs without deduplication. For
example, `SELECT * FROM east UNION ALL SELECT * FROM west` lowers both scans
into one `Concat`. Its output rows and bytes must equal the checked sum of all
input edges. CPU is one append/forward operation per output row; memory is one
output row or batch. A zero-input Concat is invalid.

### `InMemoryAnalyticWindow`

Evaluates an ordered SQL analytic function over in-memory partitions. For
example,
`ROW_NUMBER() OVER (PARTITION BY region ORDER BY latency DESC)` partitions by
region, orders each partition, and appends the row-number column. Its physical
configuration records partition keys, ordering keys, and function work per
row; evidence supplies the actual partition distribution. Ordering CPU and
memory use the same per-partition calculation as comparison sort, plus window
function work for each row. This is **not** a streaming tumbling window,
sliding window, pane layout, or exponential-histogram window framework.

### `Limit { limit, offset }`

Stops after consuming enough rows to satisfy an unordered limit. For example,
`SELECT * FROM metrics LIMIT 10 OFFSET 5` consumes at most fifteen rows and
emits at most ten. CPU is the number of consumed rows; memory is one output row
or batch. An ordered limit is represented by Sort plus Limit or a supported
Top-K implementation.

### `PassThrough`

Represents a proven row- and byte-preserving physical boundary with per-row
forwarding work. The current lowerer uses it only for a programmatically
constructed identity `TimeShift(default, Scan(metrics))`, whose query semantics
are the same as `SELECT * FROM metrics`; normal front ends omit that identity
wrapper. A non-identity PromQL `offset` or `@` changes source time coverage and
is unavailable until lowering propagates that temporal context into descendant
Scan evidence and physical identity. `PassThrough` must never hide an
unsupported operation.

### `PromqlRange { range_millis }`

Forms a range vector for each evaluation step and retains at most the observed
`max_window_samples_per_series` samples for each input series. For example,
the selector in `rate(http_requests_total[5m])` lowers to `PromqlRange` with
`range_millis = 300000`. Evidence carries series count, evaluation-step count,
value kind, and the maximum samples in one per-series range. CPU visits every
input sample; local memory is `series × max_window_samples_per_series ×
sample_width`. This query-time range buffer is not a tumbling, sliding, pane,
or exponential-histogram layout for maintaining a summary.

### `PromqlSubquery { range_millis, resolution_millis }`

Evaluates a child expression at inner steps and groups those results into a
range vector at each outer step. For example,
`max_over_time(rate(http_requests_total[5m])[1h:1m])` contains a one-hour
subquery at one-minute resolution. Evidence supplies the realized
`subquery_steps`; child steps must equal `outer_steps × subquery_steps`. CPU
charges child-result visits plus emitted results, and memory retains the
materialized inner-step input for one invocation.

### `PromqlBinary { operation, operand_mode, cardinality, build_side }`

Evaluates scalar/vector arithmetic or comparison, or a PromQL `and`, `or`, or
`unless` set operation. For example,
`rate(errors_total[5m]) / on(service) group_left(region) service_info` is a
many-to-one vector match; `up or maintenance_mode` is a set union. The node
records whether each operand is scalar or vector, vector-match cardinality,
operation class, and a hash-build side for vector/vector matching. Evidence
supplies both edge shapes and encoded matching-key width. CPU visits both
inputs and the output; vector/vector memory is the selected build-side series
times key width plus hash metadata. Operation-specific cardinality bounds are
validated (`or` may use the sum, while `and`/`unless` cannot exceed the left).

### `PromqlRelabel { expression_operations_per_row }`

Evaluates a PromQL label transformation without changing series or sample
cardinality. For example,
`label_replace(up, "host", "$1", "instance", "(.*):.*")` lowers to this
operator. The physical configuration counts expression work per sample; CPU
is rows times that count and memory is one output row or batch.

### `PromqlInfoEnrich { matcher_operations_per_info_row }`

Builds a lookup over an info metric and enriches the data vector while
preserving its left-side cardinality. For example,
`info(rate(http_server_request_duration_seconds_count[2m]))` lowers the data
expression and a separately scoped info-series `Scan`, then joins them here.
Evidence supplies both vector edges and matching-key width. CPU visits data,
info, and output rows plus selector-matcher work; memory is the info-side hash
state. Both scans must be present in the comparison scope.

### `PromqlSeriesSample { kind, grouping_key_count }`

Selects series by deterministic sampling within optional label groups. For
example, `limitk(10, up)` records `LimitK { k: 10 }`, while
`limit_ratio(0.1, up)` records the exact ratio bits. Evidence supplies group
count and encoded selection-key width. CPU visits samples and hashes input
series; memory retains selected series keys. `limitk` output is bounded by
`min(input_series, group_count × k)`. The current lowerer rejects the
unsupported `without` grouping form instead of changing its semantics.

### `PromqlScalarToVector`

Materializes one vector sample per evaluation step from a scalar, as in
`vector(1)`. Its edge contract is Scalar to a one-series Vector with the same
steps. CPU visits input and output rows; memory is one output row or batch.

### `PromqlVectorToScalar`

Converts a vector to a scalar at each evaluation step, as in `scalar(up)`.
Its edge contract is Vector to Scalar with the same steps. CPU visits input
and output rows; memory is one output row or batch.

### `PromqlScalarLeaf`

Produces a source-free scalar for each evaluation step. Number literals,
`time()`, and `pi()` are examples. It has no child or external input edge, and
its output must contain exactly one scalar row per step. CPU is one operation
per emitted scalar and memory is one scalar row.

### `PromqlPerSeries { operations_per_row, accumulator_count }`

Updates fixed-size state independently for each series and emits at most one
instant-vector sample per series and step. Examples include
`rate(http_requests_total[5m])` after `PromqlRange`, and
`avg_over_time(temperature_celsius[10m])`. The configuration records primitive
update work and accumulator count; evidence supplies accumulator bytes per
series. CPU is `input_rows × operations_per_row`; memory is
`input_series × accumulator_bytes_per_series`. Ordered or
distribution-dependent functions are unavailable until a distinct physical
algorithm and formula exist.

### `PromqlPresence { kind, operations_per_row }`

Implements presence semantics while preserving their two different
cardinality rules. `Absent` covers `absent(up)` and
`absent_over_time(up[5m])`; it may synthesize at most one series and one row
per evaluation step, and an entirely empty input emits one row per step.
`PresentPerSeries` covers `present_over_time(up[5m])`; its output is bounded by
the input series and input rows. CPU visits input and output rows; memory is
one output row or batch.

## Comparison and failure behavior

Raw and post-ASAP alternatives must cover the same `ComparisonScope`: arrival
mode, planning time, workload horizon, recurrence, event-time selection,
logical sources, ordinary predicates or info-metric matchers, and physical
source snapshots.

Missing statistics, stale evidence, an unsupported physical algorithm,
unlowered logical operations, inconsistent edges, or different comparison
scopes make the complete candidate unavailable. The integration must never
replace those failures with zero cost or structural node counting.

See [Analytical resource cost](analytical-resource-cost.md) for the resource
formulas, evidence validation, comparison-scope rules, and calibration model.
