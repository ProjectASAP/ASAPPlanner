# Analytical resource cost model

## Overview

The analytical cost model compares legal physical plans using estimated CPU
work, peak retained memory, and source/disk I/O. It does not use plan-node
count as a cost proxy.

Costing happens after semantic and accuracy validation. A low estimated cost
cannot make an illegal sketch, grouping layout, or composition selectable.
When a required statistic is unknown, the estimate is unavailable rather
than filled with a favorable default.

The model separates two concerns:

1. operator formulas produce resource estimates with physical units;
2. deployment calibration converts those dimensions into one ranking value.

The uncalibrated result is:

```text
ResourceEstimate {
    cpu_ops,
    peak_memory_bytes,
    scan_bytes,
}
```

`cpu_ops` is an algorithmic operation count, not elapsed time.
`peak_memory_bytes` is the maximum retained/working state represented by the
plan. `scan_bytes` counts source or disk data read; consuming an in-memory
intermediate does not create another source scan.

## Comparison scope

Every alternative must cover the same source snapshot and workload horizon.
The current analytical model is a **build-once, read-many** model:

```text
build_count = 1
query reads = evaluation_count
updates after build = 0
```

The sketch alternative scans the snapshot once to build retained state. The
raw alternative scans the same snapshot for every evaluation. Incremental
ingestion, rebuilds, expiration/deletion, and lifecycle duration require an
update-aware model and are not inferred from `evaluation_count`.

### Canonical workload sources

The cost model does not define a second workload schema. Planner inputs come
from the normalized workload and the lowered query IR:

| Cost concept | Canonical source |
|---|---|
| Source row cardinality | Fresh `DataWorkload.input_cardinality: Evidence<u64>`. |
| Data distribution | Fresh `DataWorkload.distribution: Evidence<DataDistribution>`. |
| Data arrival and update rate | `DataWorkload.arrival`, `ingestion_volume`, and `ingestion_rate`. The build-once model accepts only the at-rest/no-update case. |
| Query evaluations | `QueryWorkloadEntry.recurrence`, normalized over an explicit horizon. One-time invocations, schedules, fixed intervals, and demand estimates retain their existing semantics. |
| Event-time scope | `QueryWorkloadEntry.time_selection`; lookback and `as_of` determine which snapshot/window is costed. |
| Accuracy and latency constraints | `QueryWorkloadEntry.requirements`. |
| Top-K `k`, grouping keys, and operator shape | The lowered `QueryExpr`/`AggIntent`; these are never independently re-declared as workload facts. |

Every `Evidence<T>` value is read through its freshness contract at planning
time. Stale, future, or provenance-free time-bounded evidence remains
unknown. Costing must not bypass the same freshness rules used by accuracy
and lifecycle planning.

`evaluation_count` is therefore a derived value, not an independent semantic
axis. For a finite horizon it is computed from `QueryRecurrence`; if a
repeated demand cannot be converted to a finite count without a horizon, the
total-cost comparison is unavailable.

### Statistics not yet present in the workload schema

Some physical estimates need facts that the current `DataWorkload` does not
carry:

| Missing evidence | Why it is needed | Correct ownership |
|---|---|---|
| Source byte size / average row width | Source scan bytes and row buffers. | Fresh data/catalog evidence. |
| Distinct cardinality for a particular grouping-key tuple | Hash-aggregation state and per-subpopulation layouts. | Per-column/key-set statistics keyed by the lowered grouping expression. |
| Average encoded grouping-key width | Exact hash entries and intermediate row width. | Schema/catalog statistics. |
| Filter selectivity and operator output cardinality | Parent CPU and memory estimates. | Plan-node statistics derived from data evidence. |
| Join-side and join-output cardinality | Build-side selection, hash state, and parent cardinality. | Plan-node statistics derived from join-key evidence. |
| Spill, cache, and network behavior | External/distributed physical alternatives. | Deployment/execution-profile evidence. |

These facts should be added as freshness-aware workload/catalog evidence or
provided by an operator-statistics provider. They must not become a parallel
user-authored cost-model workload format.

`AnalyticalInputs` and `OperatorInputs` are currently flattened estimator
adapters. Their fields (`group_count`, key bytes, output rows/bytes, and join
side statistics) are formula arguments after canonical workload/IR evidence
has been resolved. They are not new planner domain objects. Likewise,
`dag_export --analytical-cost-json` is a development and reproducibility
adapter; it is not the normalized planner API.

The aggregation/Top-K adapter derives the intermediate cardinality it can
prove: a grouped count produces `group_count` rows of approximately
`group_key_bytes + 8` bytes each. Arbitrary filters, joins, and projections
require supplied statistics; the model does not invent selectivity or output
width.

`group_count` and `input_rows` are different quantities. One hundred million
rows may contain one hundred thousand distinct `service` values; that input
has `input_rows = 100_000_000` and `group_count = 100_000`.

`input_bytes` and `source_scan_bytes` are also different. An outer operator
may consume a 4 MB intermediate while causing zero additional disk reads.
Keeping them separate prevents nested operators from charging the original
source scan more than once.

Required counts and widths must be positive. `source_scan_bytes` may be zero
for an intermediate.

Some costs need evidence beyond cardinality:

- CMSWithHeap/CountSketchWithHeap legality needs a Top-K margin certificate;
- DDSketch memory needs value-range/distribution evidence;
- shared sketch layouts need their physical sketch count or grouping model;
- external operators need memory budget and spill read/write estimates;
- distributed operators need network bytes and a network calibration axis.

The current scalar objective includes CPU, retained memory, and source/disk
reads. It does not yet include source writes, spill I/O, network transfer,
parallelism, cache residency, allocator fragmentation, or wall-clock
critical-path latency. A physical plan that depends on one of those effects
must supply an extended model rather than treating the missing dimension as
zero.

## Operator-local estimates

An operator estimate excludes child work. Plan composition counts each DAG
node once, adds cumulative CPU and disk work, and combines state that must
coexist. Streaming operators contribute only their working buffer; retained
summaries remain live across evaluations.

The physical operator API defines these local formulas:

| Operator | CPU operations | Peak memory | Source/disk scan |
|---|---:|---:|---:|
| Scan | `input_rows` | one input row | `input_bytes` |
| Filter | `input_rows` | one output row | `0` |
| Project/pass-through | `input_rows` | one output row | `0` |
| Hash aggregate | `input_rows` | `groups × (key bytes + value bytes + hash metadata)` | `0` |
| Deduplicate | `input_rows` | keyed state, as for hash aggregation | `0` |
| Full in-memory sort | `rows × ceil(log2(rows))` | `input_bytes` | `0` |
| Heap Top-K | `rows × ceil(log2(max(k, 2)))` | `min(k, rows) × row_bytes` | `0` |
| Hash join | `left rows + right rows + output rows` | chosen build-side bytes | `0` beyond child scans |
| Concat | `output_rows` | one output row | `0` |
| Ordered window | `rows × ceil(log2(rows))` | partition/input bytes | `0` |
| Limit | `output_rows` | one output row | `0` |

A logical operator is not automatically a complete physical specification.
For example, an external sort must explicitly add spill writes and reads; it
must not reuse the in-memory-sort formula and call the result disk-aware.
Likewise, a nested-loop join needs a different CPU expression from a hash
join. Missing output cardinality, selectivity, row width, or join-side
statistics makes the affected estimate unavailable.

## Exact grouped aggregation and Top-K

An exact grouped count materializes all groups in a hash table. The model
uses an 8-byte count and 16 bytes of hash-table metadata per group:

```text
group_entry_bytes = group_key_bytes + 8 + 16

aggregation_cpu_per_evaluation = input_rows
aggregation_memory             = group_count × group_entry_bytes
```

For exact Top-K over those groups:

```text
topk_cpu_per_evaluation = group_count × ceil(log2(max(k, 2)))
topk_memory             = min(k, group_count) × (group_key_bytes + 8)
```

The hash table remains present while Top-K selection runs, so the heap is
additional memory rather than a replacement for the materialized groups.
Across a horizon of `evaluation_count = E`:

```text
cpu_ops = E × (aggregation_cpu_per_evaluation + topk_cpu_per_evaluation)
scan_bytes = E × source_scan_bytes
peak_memory_bytes = aggregation_memory + topk_memory
```

The per-entry sizes are explicit logical-layout assumptions. They do not
claim to include every allocator or execution-engine object overhead.

## Sketch resource formulas

A retained sketch scans its source once during construction. Later query
evaluations read the sketch state:

```text
cpu_ops = input_rows × update_ops(params)
        + evaluation_count × physical_sketch_count × read_ops(params)

scan_bytes = source_scan_bytes
```

For a per-subpopulation layout, `physical_sketch_count = group_count`. A
shared multi-subpopulation structure has one physical sketch. A global keyed
heavy-hitter sketch also has one physical sketch; its keys live in the sketch
heap and it does not allocate a map from group to sketch instance.

Concrete accuracy-sized parameters determine work and state:

| Sketch | Update operations per input row | Read operations per evaluation | State bytes per sketch |
|---|---:|---:|---:|
| CMS | `depth` | `depth` | `width × depth × 8` |
| CountSketch | `depth` | `depth` | `width × depth × 8` |
| CMSWithHeap | `depth + ceil(log2(heap_size))` | `depth + heap_size` | `width × depth × 8 + heap_size × 16` |
| CountSketchWithHeap | `depth + ceil(log2(heap_size))` | `depth + heap_size` | `width × depth × 8 + heap_size × 16` |
| KLL | `1 + ceil(log2(k))` | `ceil(log2(k))` | `k × 8` |
| HLL | `1` | `2^precision` | `2^precision` |
| KMV | `ceil(log2(k))` | `k` | `k × 8` |
| Theta | `ceil(log2(k))` | `k` | `k × 8` |

Logarithms use an argument of at least two. An algorithm/parameter mismatch
is an error, not permission to reinterpret the parameters.

DDSketch is unavailable because its occupied-bin count depends on the input
value distribution and range. Estimating it requires that evidence; the model
does not invent a bin count.

## Count-ranked Top-K fusion

SQL count-ranked Top-K is canonically represented as two logical aggregates:

```sql
SELECT service, COUNT(*) AS frequency
FROM metrics
GROUP BY service
ORDER BY frequency DESC
LIMIT 10;
```

```text
TopK(Count GROUP BY service)
```

Those logical layers must not become one CMS per service followed by another
sketch. A heavy-hitter sketch counts keys directly from the raw stream.
Candidate construction therefore fuses the pattern into:

```text
Scan(metrics)
    -> CMSWithHeap(key=service, heap_size=10)
    -> TopK readout
```

For `width = 272`, `depth = 5`, and `heap_size = 10`, retained state is:

```text
CMS counters = 272 × 5 × 8 = 10,880 bytes
heap         = 10 × 16      =    160 bytes
total                           11,040 bytes
```

This is one global sketch, independent of `group_count`. `group_count` still
matters to the exact baseline because it determines how many exact group
entries must be materialized.

## Top-K accuracy evidence

Approximate Top-K membership needs more than a point-count error bound. The
planner requires a margin certificate containing:

- a lower confidence bound for the kth selected item;
- the greatest upper confidence bound among excluded items;
- the union-bounded failure probability of those intervals.

The selected lower bound must exceed the excluded upper bound. Without this
separation, CMSWithHeap and CountSketchWithHeap remain illegal regardless of
their estimated cost. `dag_export --topk-margin-json` supplies this evidence
for an export run.

## Workload-horizon example

Assume:

```text
input rows       = 100,000,000
source size      = 6.4 GB
distinct service = 100,000
service key      = 32 bytes
evaluations      = 100
k                = 10
```

The scan column is cumulative across the horizon:

```text
pre-ASAP scan  = 6.4 GB/evaluation × 100 evaluations = 640 GB
post-ASAP scan = 6.4 GB/build × 1 retained build      = 6.4 GB
```

Pre-ASAP has no retained summary in this alternative, so it rebuilds the
hash aggregation and exact Top-K for every evaluation. Post-ASAP builds one
CMSWithHeap and subsequent evaluations read its 11,040-byte state. The model
does not describe post-ASAP as having free construction: its initial scan and
update CPU are included.

With calibration coefficients:

```text
cost_per_cpu_op       = 0.000001
cost_per_scan_byte    = 0.00000001
cost_per_retained_byte = 0.000000001
```

the comparison is:

| Complete plan | CPU ops | Peak memory | Source scan | Calibrated cost |
|---|---:|---:|---:|---:|
| Exact hash aggregation + heap Top-K | 10,040,000,000 | 5,600,400 B | 640,000,000,000 B | 16,440.0056004 |
| One global CMSWithHeap | 900,001,500 | 11,040 B | 6,400,000,000 B | 964.00151104 |

The result depends on both the stated horizon and calibration. With one
evaluation, both alternatives scan the source once; the repeated-scan benefit
does not exist. A different CPU/memory/I/O policy may also change the ranking.

## Calibration

Resource dimensions become one planner objective only through an explicit,
versioned deployment calibration:

```text
cost = cpu_ops × cost_per_cpu_op
     + scan_bytes × cost_per_scan_byte
     + peak_memory_bytes × cost_per_retained_byte
```

Coefficients must be finite and non-negative, and at least one must be
positive. The scalar is reported as `CostUnits`; it is not implicitly money,
latency, or cost per second. Coefficients may come from benchmarks or an
explicit resource policy. They are not universal hardware constants.

The calibration version is exported with the model version. Estimates using
different calibration versions must not be treated as directly comparable.

## Selection and exported provenance

For each legal sketch candidate, the planner:

1. resolves the accuracy target;
2. sizes the algorithm for that target;
3. estimates operator resources using the candidate's concrete parameters;
4. composes nested DAG resources without recharging shared scans;
5. applies calibration and ranks candidates by ascending cost.

The raw baseline and selected plan use the same workload scope and
calibration:

```text
benefit       = baseline_cost - selected_cost
benefit_ratio = benefit / baseline_cost
```

Exported annotations include workload/query inputs, CPU operations, peak
memory, source-scan bytes, calibration coefficients, and model/calibration
versions. This is enough to reproduce the scalar and explain which resource
dimension drove the decision.

## Failure behavior

An estimate is unavailable when:

- a required workload or operator statistic is missing or zero;
- calibration is negative, non-finite, or entirely zero;
- sketch parameters do not match the algorithm;
- required distribution or Top-K margin evidence is absent;
- a candidate or physical implementation is unsupported;
- checked arithmetic overflows.

Structural node counts are never used as a fallback. Consumers render an
unavailable estimate as `Not estimated`; unavailable never means zero cost.
