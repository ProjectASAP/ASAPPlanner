# Physical operators and evidence

Audience: developers extending lowering, statistics validation, and analytical
costing. Read the [physical integration boundary](../design_docs/architecture/physical-plan-integration.md)
first. This reference describes the supported operator vocabulary and evidence
requirements; runtime deployment remains downstream.

Sources: [physical types](../../crates/types/src/resources/physical.rs),
[query lowering](../../crates/asap-aware-mapping/src/query_physical_lowering.rs),
and [analytical estimator](../../crates/asap-aware-mapping/src/analytical_cost.rs).

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
