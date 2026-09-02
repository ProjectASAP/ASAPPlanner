# Analytical resource cost model

## Purpose

The analytical resource cost model compares an exact aggregation over raw
data with aggregation backed by a retained sketch. It estimates physical
work instead of using the number of plan nodes as a proxy.

The model has two layers:

1. estimate CPU work, peak retained memory, and input scan I/O independently;
2. apply a deployment-provided calibration to obtain one scalar used for
   candidate ranking.

Keeping the resource dimensions in the estimate and its exported provenance
makes the result explainable and allows different deployments to apply
different priorities without changing the underlying complexity formulas.

The model compares alternatives that have already passed semantic, accuracy,
and lifecycle legality checks. Cost estimation cannot make an illegal plan
legal.

## Inputs and comparison scope

An `AnalyticalCostModel` contains workload/query-shape inputs and a resource
calibration. Required numeric inputs are positive integers:

| Input | Meaning |
|---|---|
| `input_rows` | Number of rows processed when the source is scanned once. |
| `input_bytes` | Logical bytes consumed by the operator region, including intermediate input. |
| `source_scan_bytes` | Source/disk bytes read once for this operator region. This is zero when an operator consumes an already-materialized intermediate. |
| `group_count` | Estimated number of distinct aggregation groups. With no `GROUP BY`, this is `1`; for `GROUP BY service, region`, it is the estimated number of distinct `(service, region)` pairs. |
| `group_key_bytes` | Average encoded bytes of one distinct grouping-key tuple. This must include the key payload, but not the model's separately counted hash-table metadata. |
| `topk_k` | Optional `k` for an outer `ORDER BY aggregate DESC LIMIT k`; absent when the comparison has no Top-K. |
| `evaluation_count` | Number of query evaluations in the comparison scope. |

The comparison scope must be the same for every candidate. For example, if a
workload evaluates a query 100 times during the chosen horizon, both the raw
and sketch alternatives use `evaluation_count = 100`.

`group_count` is not the input row count. One million input rows containing
ten distinct grouping keys have `input_rows = 1_000_000` and
`group_count = 10`. It may come from catalog distinct-count statistics,
observed workload data, or another estimator. When a shared multi-subpopulation
sketch represents all groups in one physical structure, the estimator counts
one retained sketch rather than one independent sketch per group.

Zero or missing inputs are invalid. Unknown information must not be replaced
with an arbitrary default.

## Resource estimate

The uncalibrated result is:

```text
ResourceEstimate {
    cpu_ops,
    peak_memory_bytes,
    scan_bytes,
}
```

`cpu_ops` is an algorithmic operation count, not elapsed CPU time.
`peak_memory_bytes` is the retained state needed in the comparison scope.
`scan_bytes` counts source data read, not reads of the already retained
sketch. These dimensions are intentionally not added together before
calibration because their units differ.

All arithmetic is checked. An estimate that overflows fails closed.

## Physical operator formulas

Plan costing separates a node's own work from its children. A DAG walk adds
CPU and disk once per physical node; retained states that coexist are added,
while a streaming operator contributes only its working row/buffer. The
operator classes and local formulas are:

| Physical operator | CPU operations | Peak working/retained memory | Disk/source scan |
|---|---:|---:|---:|
| Scan | `input_rows` | one input row | `input_bytes` |
| Filter | `input_rows` | one output row | `0` |
| Project / scalar pass-through | `input_rows` | one output row | `0` |
| Hash aggregate | `input_rows` | `groups * (key_bytes + value_bytes + hash_metadata)` | `0` |
| Deduplicate | `input_rows` | same keyed-state formula as hash aggregate | `0` |
| Full sort | `rows * ceil(log2(rows))` | `input_bytes` | `0` unless an external-sort implementation is selected |
| Top-K heap | `rows * ceil(log2(k))` | `min(k, rows) * row_bytes` | `0` |
| Hash join | `left_rows + right_rows + output_rows` | bytes of the chosen build side | `0` beyond child scans |
| Concat | `output_rows` | one output row | `0` |
| Window | `rows * ceil(log2(rows))` when ordering is required | partition/input bytes | `0` |
| Limit | `output_rows` | one output row | `0` |

A logical operator does not determine every physical behavior. For example,
an external sort would add spill reads/writes, and a nested-loop join would
have a different CPU formula. Such a physical alternative must be represented
explicitly; the model does not silently charge an in-memory sort while calling
it disk-aware. Filter selectivity, output cardinality/width, join-side
cardinality, and similar required statistics fail closed when absent.

## Exact raw aggregation baseline

The baseline recomputes the aggregation from raw input for every evaluation.
The current grouped-aggregation model assumes one unit of CPU work per input
row and a 16-byte accumulator/key slot per group:

```text
cpu_ops           = input_rows * evaluation_count
                  + topk_cpu_ops
scan_bytes        = source_scan_bytes * evaluation_count
peak_memory_bytes = group_count * (group_key_bytes + 8 + 16)
                  + topk_heap_bytes
```

The per-group state includes the grouping key, an 8-byte exact value, and 16
bytes of hash-table metadata. This is a documented logical-layout assumption,
not a claim about every execution engine's allocator overhead.

For `topk_k = k`, selection is modeled with a size-`k` heap over every
materialized group on every evaluation:

```text
topk_cpu_ops  = evaluation_count * group_count * ceil(log2(max(k, 2)))
topk_heap_bytes = min(k, group_count) * (group_key_bytes + 8)
```

This explicitly accounts for retaining all groups before Top-K selection;
the heap is additional working memory, not a substitute for the group table.

## Sketch-backed aggregation

The sketch alternative builds retained state during one complete input scan,
then serves each evaluation by reading that state:

```text
cpu_ops = input_rows * update_ops(params)
        + evaluation_count * physical_sketch_count * read_ops(params)
        + topk_cpu_ops

scan_bytes        = source_scan_bytes
peak_memory_bytes = physical_sketch_count * state_bytes(params)
                  + group_count * (group_key_bytes + 16)
                  + topk_heap_bytes
```

For the ordinary per-subpopulation layout,
`physical_sketch_count = group_count`. For a shared multi-subpopulation
layout, `physical_sketch_count = 1`.

The estimator uses the concrete sketch parameters produced for the query's
accuracy target. The formulas are:

| Sketch | Update operations per row | Read operations per evaluation | State bytes per physical sketch |
|---|---:|---:|---:|
| CMS | `depth` | `depth` | `width * depth * 8` |
| CountSketch | `depth` | `depth` | `width * depth * 8` |
| CMS with heap | `depth + ceil(log2(heap_size))` | `depth + heap_size` | `width * depth * 8 + heap_size * 16` |
| CountSketch with heap | `depth + ceil(log2(heap_size))` | `depth + heap_size` | `width * depth * 8 + heap_size * 16` |
| KLL | `1 + ceil(log2(k))` | `ceil(log2(k))` | `k * 8` |
| HLL | `1` | `2^precision` | `2^precision` |
| KMV | `ceil(log2(k))` | `k` | `k * 8` |
| Theta | `ceil(log2(k))` | `k` | `k * 8` |

Logarithms use an argument of at least two so zero operations are never
introduced by a degenerate parameter. Parameter variants must match their
algorithm; a mismatch is an unavailable estimate rather than an inferred
conversion.

DDSketch is not estimated because its retained bin count depends on the input
value distribution and range. Until those statistics are part of the model,
inventing a bin count would make both memory and read cost unsound.

## Calibration and scalar objective

A `ResourceCalibration` contains three finite, non-negative coefficients and
a required version string:

```text
cost = cpu_ops * cost_per_cpu_op
     + scan_bytes * cost_per_scan_byte
     + peak_memory_bytes * cost_per_retained_byte
```

At least one coefficient must be positive. The scalar is reported in
`CostUnits`; it is not implicitly money, latency, or cost per second. The
deployment defines that meaning through its calibration procedure and
version. For example, coefficients may be fitted from benchmark measurements
or chosen to express a resource budget. They are never hard-coded as
universal hardware constants.

Changing any coefficient can change the selected candidate. This is expected:
a memory-constrained deployment and a scan-constrained deployment need not
prefer the same sketch. The calibration version is included in exported
provenance so two estimates produced under different policies are not
mistaken for directly comparable results.

## Candidate selection

For an aggregation with several legal sketch algorithms, the planner:

1. resolves the declared accuracy target;
2. sizes each algorithm for that target;
3. computes its resource estimate using the same workload inputs;
4. applies the calibration;
5. orders supported candidates by ascending calibrated cost.

Candidate enumeration remains exhaustive. An unavailable estimate is not a
reason to claim a numerical cost, and cost ranking never bypasses accuracy or
semantic validation.

Approximate Top-K additionally requires a margin certificate: a lower bound
for the kth selected item, an upper bound for every excluded item, and their
union-bounded failure probability. The selected lower bound must exceed the
excluded upper bound. `dag_export --topk-margin-json` supplies this evidence;
without it, CMSWithHeap/CountSketchWithHeap remain illegal and the planner
keeps an exact Top-K rather than using cost to override correctness.

The raw baseline and selected sketch cost use the same workload scope and
calibration. Their exported benefit is:

```text
benefit       = baseline_cost - selected_cost
benefit_ratio = benefit / baseline_cost
```

A positive benefit means the selected post-ASAP alternative is cheaper than
raw pre-ASAP recomputation under the stated inputs and calibration.

## Exported provenance

`dag_export` accepts a serialized model through
`--analytical-cost-json`. Its baseline, selected, and benefit annotations use
model version `analytical-resource-v1` together with the calibration version.
The annotation inputs include:

- all workload and query-shape inputs;
- estimated CPU operations, peak memory, and scan bytes;
- all three calibration coefficients and their units.

This is sufficient to reproduce the scalar from the exported annotation and
to explain which resource dimension drove a decision.

## Failure behavior

The model returns an unavailable estimate when any of the following holds:

- a workload input is missing or zero;
- a calibration coefficient is negative, non-finite, or all coefficients are
  zero;
- sketch parameters do not match the sketch algorithm;
- required distribution evidence is absent;
- the candidate shape is unsupported;
- checked arithmetic overflows.

Structural node counts are never used as a fallback. They have no physical
unit and can reverse the conclusion of a CPU, memory, and scan comparison.
Consumers render unavailable estimates as `Not estimated` and must not infer
a zero cost.
