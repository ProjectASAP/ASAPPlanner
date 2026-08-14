# Pre-ASAP IR

The goal of the pre-ASAP IR is represent operations from different query languages in a single representation, and make it easier to analyze how/where ASAP primitives can be used.
Only operations that are semantically relevant to answering the query and selecting an ASAP primitive need to become first-class nodes here.

Design principles:

1. Expose the query semantics that affect summary applicability, correctness, and cost.
2. If an operation changes presentation but does not change the semantic summary intent, it does not need to be represented.
3. Equivalent SQL, PromQL, and future-language queries should produce the same intent shape.

The pre-ASAP IR is defined using the `QueryExpr` enum. We discuss some of important enum types below.

## Node index

Grouped to match the sections below — common relational nodes first, then the nodes specific
to one source language.

**[Aggregation-related nodes](#aggregation-related-nodes)**
- [`Aggregate`](#aggregate) — collapses input rows into fewer output rows via grouping dimensions and measures.

**[Time-related nodes](#time-related-nodes)**
- [`TimeRange`](#timerange) — a range-vector lookback over the time axis (PromQL `[5m]`).
- [`TimeShift`](#timeshift) — shifts *when* a selector is evaluated (PromQL `offset`/`@`).
- [`Subquery`](#subquery) — re-evaluates an instant-vector expression over a range at a given step.

**[Relational nodes](#relational-nodes)** — common to both SQL and PromQL
- [`Scan`](#scan) — identifies the logical data source.
- [`Filter`](#filter) — restricts rows using a predicate.
- [`Project`](#project) — column projection (SQL `SELECT` list).
- [`BinaryOp`](#binaryop) — arithmetic / comparison / boolean composition of two inputs.
- [`Sort`](#sort) — generic (non-heavy-hitter) order-by, optionally per-group.
- [`Limit`](#limit) — caps the row count, with an offset.
- [`Distinct`](#distinct) — row-level deduplication.
- [`Join`](#join) — logical join of two inputs.
- [`SetOp`](#setop) — SQL's typed set operations (`UNION`/`INTERSECT`/`EXCEPT`).
- [`Merge`](#merge) — exact, untyped `UNION ALL` of union-compatible branches.

**[PromQL-specific nodes](#promql-specific-nodes)**
- [`Scalar`](#scalar) — a scalar constant leaf.
- [`EvalTime`](#evaltime) — the query evaluation time as a scalar (PromQL `time()`).
- [`VectorFromScalar`](#vectorfromscalar) — promotes a scalar to a label-less instant vector.
- [`ScalarFromVector`](#scalarfromvector) — collapses a single-series vector to a scalar.
- [`Relabel`](#relabel) — per-series label rewrite (PromQL `label_replace`/`label_join`).
- [`InfoJoin`](#infojoin) — left-join label enrichment from an info metric.
- [`Sample`](#sample) — keeps a subset of whole series, not a reduction.

**[SQL-specific nodes](#sql-specific-nodes)**
- [`WindowFunc`](#windowfunc) — SQL analytic window function (`OVER (...)`).

## Aggregation-related nodes

### Aggregate

```text
QueryExpr::Aggregate(
    input = ...,
    group_by = [FieldRef("service")],
    measures = [Count]
)
```

#### Grouping dimensions

`group_by` contains semantic `FieldRef`s, not schema positions.

```text
group_by = [FieldRef("service"), FieldRef("region")]
```

#### Measures

Measures describe what statistic or aggregate is required. At minimum the algebra should
support the following semantic measures:

```text
Count
Sum(field)
Min(field)
Max(field)
Avg(field)
Quantile(field, q)
DistinctCount(field)
```

Additional measures can be added when there is a stable semantic distinction and a
meaningful summary implementation.

**Fields** (real implementation — differs from the sketch above, see #185):
- `reduction` — whether this is a genuine cross-entity reduction (`Reduce(GroupKeys)`) or a per-entity pass-through with no grouping concept at all (`PerEntity`).
- `aggs` — the aggregate intents to compute (`Sum`, `Rate`, `HistogramQuantile`, ...).
- `output_names` — output column name per entry in `aggs`; overrides the synthetic default when non-empty.
- `child` — the input being aggregated.

## Time-related nodes

### TimeRange

Represents a range of time. Kept different from `Filter` to treat time as an explicit concern.

```promql
rate(http_requests_total[5m])
```

**Fields:**
- `range` — how far back to look (the PromQL `[5m]` duration).
- `child` — the input the range applies to.

### TimeShift

PromQL `offset` / `@` time shift on a selector — a pass-through wrapper that moves *when*
the child is evaluated but leaves its schema unchanged.

```promql
up offset 5m
```

**Fields:**
- `shift` — the offset/`@` anchor to apply (moves *when* `child` is evaluated).
- `child` — the selector being shifted.

### Subquery

PromQL sub-query syntax `<expr>[range:resolution]` — a logical pass-through that lets a
range function apply to the result of an already-evaluated instant-vector expression at a
given step resolution.

```promql
avg_over_time(up[5m:1m])
```

**Fields:**
- `range` — how far back the sub-query evaluates.
- `resolution` — the step between evaluated points; `None` defers to the default step.
- `child` — the instant-vector expression being re-evaluated over the range.

## Relational nodes

### Scan

Identifies the logical data source.

```text
Scan(
    source = "metrics"
)
```

PromQL metric selection and SQL `FROM` clauses can both map into `Scan` when they denote
the same logical data domain.

**Fields:**
- `source` — the logical data source (a table name or PromQL metric selector).
- `predicates` — row-level filters pushed all the way down to this scan.
- `schema` — the binding schema every positional column reference in the tree resolves against.

### Filter

Restricts the logical input using predicates.

```text
Filter(
    input = Scan("metrics"),
    predicate = region = "us-east"
)
```

Filters are first-class because they can change which summaries are applicable. A summary
for an entire dataset is not necessarily sufficient to answer the same intent under an
arbitrary predicate.

Note: a `WHERE`/label-matcher predicate that pushes all the way onto a base table scan
lands in `Scan.predicates` instead of a separate `Filter` node — `Filter` only survives when
the predicate sits over something a scan can't absorb, e.g. a post-aggregate column:

```sql
SELECT * FROM (SELECT srcip, COUNT(*) AS cnt FROM packets GROUP BY srcip) t WHERE cnt > 10
```

**Fields:**
- `pred` — the row-level predicate to apply.
- `child` — the input being filtered.

### Project

π — column projection (SQL `SELECT` list). No PromQL equivalent: PromQL never subsets
columns, so this node is SQL-only in practice today.

```sql
SELECT srcip, dstip FROM packets
```

**Fields:**
- `cols` — the output column list (expression + optional alias per column).
- `qualifier` — a table alias re-qualifying every output column, for a derived table; `None` for an ordinary `SELECT` list.
- `child` — the input being projected.

### BinaryOp

Arithmetic / comparison / boolean composition. PromQL binary operators between two vectors,
a vector and a scalar, or two scalars.

```promql
up > 1
```

**Fields:**
- `op` — the arithmetic/comparison/boolean operator.
- `lhs` — the left operand.
- `rhs` — the right operand.
- `vector_match` — PromQL vector-matching modifiers (`on`/`ignoring`, `group_left`/`group_right`); `None` outside PromQL.

### Sort

Generic order-by for non-heavy-hitter cases. `partition_by` makes the ordering per-group —
the home for PromQL `topk by (...)`/SQL `... OVER (PARTITION BY ...)`-style grouped ranking.

```promql
sort_desc(up)
```

**Fields:**
- `keys` — the ordering columns/expressions and direction.
- `partition_by` — grouping keys that make the ordering per-group instead of global; empty = a single global order.
- `child` — the input being ordered.

### Limit

Caps the row count, with an offset. SQL `LIMIT n [OFFSET o]`; also paired with `Sort` for
PromQL's generic (non-heavy-hitter) `topk`/`bottomk`.

```promql
topk(3, up)
```

**Fields:**
- `n` — the maximum number of rows to keep.
- `offset` — how many leading rows to skip first.
- `child` — the input being capped.

### Distinct

δ — row-level deduplication (SQL `SELECT DISTINCT`). Distinct from `AggIntent::Cardinality`
(`COUNT(DISTINCT col)`), which collapses to a single number — `Distinct` still returns
multiple rows.

```sql
SELECT DISTINCT srcip, dstip FROM packets
```

**Fields:**
- `cols` — the columns to dedup on; empty = dedup on every column.
- `child` — the input being deduplicated.

### Join

Logical join; the physical strategy (hash/merge/broadcast) is picked at L4. SQL `JOIN`.

```sql
SELECT u.prefix FROM bgp_updates u JOIN bgp_rib_state r ON u.prefix = r.prefix
```

**Fields:**
- `kind` — the join type (inner/left/right/full/semi/anti).
- `pred` — the join condition.
- `left` — the left input.
- `right` — the right input.

### SetOp

SQL's typed set operations — `UNION`/`INTERSECT`/`EXCEPT` — as opposed to `Merge`'s untyped
concatenation. `UNION` (dedup) further wraps a `SetOp` in a `Distinct`; `UNION ALL` does not.

```sql
SELECT srcip FROM packets UNION ALL SELECT dstip FROM packets
```

**Fields:**
- `kind` — which set operation (union/intersect/except).
- `all` — whether duplicates are kept (`ALL`) or removed.
- `left` — the left branch.
- `right` — the right branch.

### Merge

⊕ — exact, n-ary `UNION ALL` of independent, union-compatible branches; rows concatenate,
never dedup. Used when a single `Aggregate` can't express the shape — the canonical case is
PromQL `histogram_quantiles` (one branch per φ, each its own `HistogramQuantile` reduction
relabeled with its `le` value) — and SQL `ROLLUP`/`CUBE`/`GROUPING SETS` (one branch per
grouping level).

```promql
histogram_quantiles(rate(http_request_duration_seconds_bucket[5m]), "le", 0.5, 0.9)
```

**Fields:**
- `children` — the union-compatible branches to concatenate; must be non-empty.

## PromQL-specific nodes

### Scalar

A scalar constant leaf — a PromQL number literal, or a folded constant scalar expression.
Appears as a `BinaryOp` operand for `<vector> op <scalar>` thresholds and unit conversions.

```promql
up > 1
```

**Fields:** a single unnamed `f64` — the constant value.

### EvalTime

The query **evaluation time** as a scalar — PromQL `time()` — and the implicit input of the
no-argument calendar functions (`hour()`, `day_of_week()`, ...).

```promql
time()
```

### VectorFromScalar

The scalar→instant-vector bridge — PromQL `vector(s)`. Promotes a scalar-typed child to a
single label-less series carrying that value at every step, e.g. for dead-man's-switch
patterns (`up or vector(0)`).

```promql
vector(1)
```

**Fields:** a single unnamed child `QueryExpr` — the scalar-typed expression being promoted to a vector.

### ScalarFromVector

The instant-vector→scalar bridge — PromQL `scalar(v)`. Collapses a single-element vector to
its value (NaN at runtime if the input isn't exactly one series).

```promql
scalar(up)
```

**Fields:** a single unnamed child `QueryExpr` — the single-series vector being collapsed to a scalar.

### Relabel

ρ — a per-series label rewrite. PromQL `label_replace`/`label_join`; every row passes
through unchanged except for the destination label, whose new value is computed from the
child's label columns.

```promql
label_replace(up, "foo", "$1", "bar", "(.*)")
```

**Fields:**
- `dst` — the label being written.
- `value` — the scalar expression computing the new label value from the child's labels.
- `child` — the input series being relabeled.

### InfoJoin

PromQL `info(v, [selector])` — left-join label enrichment. Each series in the child is
enriched with labels from the matching info metric(s) (`target_info` by default).

```promql
info(up)
```

**Fields:**
- `selector` — matchers picking which info metric(s) to enrich from; empty = the default `target_info`.
- `child` — the input series being enriched.

### Sample

Series-sampling selection — PromQL `limitk`/`limit_ratio`. Keeps a subset of whole series
per group (or globally); not a ranking (`TopK`) and not a reduction, since the output schema
equals the child's.

```promql
limitk(3, up)
```

**Fields:**
- `by` — grouping keys the sample is taken within; empty = a global sample.
- `kind` — the sampling strategy (`LimitK`/`LimitRatio`) and its parameter.
- `child` — the input series being sampled.

## SQL-specific nodes

### WindowFunc

SQL analytic window function: `func(args) OVER (PARTITION BY ... ORDER BY ...)`. Output
schema is the child schema plus one new column for the window expression's result.

```sql
SELECT srcip, LAG(time) OVER (PARTITION BY srcip ORDER BY time) FROM packets
```

**Fields:**
- `func` — the window function (e.g. `LAG`, `RANK`).
- `args` — the function's operand expressions; empty for rank-only functions.
- `partition_by` — grouping keys the window is computed within.
- `order_by` — the ordering the window function reads.
- `output_name` — the name of the new output column.
- `child` — the input the window function is computed over.
