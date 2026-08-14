# Pre-ASAP IR

The pre-ASAP IR is deliberately narrower than SQL or any other source-language AST.
Only concepts that are semantically relevant to answering the query and selecting a summary
need to become first-class nodes.

Design principles:

1. Expose the query semantics dimensions that affect summary applicability, correctness, and cost.
2. If an operation changes presentation but does not change the semantic summary intent, it does not need to be represented.
3. Equivalent SQL, PromQL, and future-language queries should produce the same intent shape.

The pre-ASAP IR is defined using the `QueryExpr` enum. We discuss some of important enum types below.

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

## Time-related nodes


### TimeRange

Represents a range of time. Kept different from `Filter` to treat time as an explicit concern.

```promql
rate(http_requests_total[5m])
```

### TimeShift

PromQL `offset` / `@` time shift on a selector — a pass-through wrapper that moves *when*
the child is evaluated but leaves its schema unchanged.

```promql
up offset 5m
```

### Subquery

PromQL sub-query syntax `<expr>[range:resolution]` — a logical pass-through that lets a
range function apply to the result of an already-evaluated instant-vector expression at a
given step resolution.

```promql
avg_over_time(up[5m:1m])
```

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

### Project

π — column projection (SQL `SELECT` list). No PromQL equivalent: PromQL never subsets
columns, so this node is SQL-only in practice today.

```sql
SELECT srcip, dstip FROM packets
```

### BinaryOp

Arithmetic / comparison / boolean composition. PromQL binary operators between two vectors,
a vector and a scalar, or two scalars.

```promql
up > 1
```

### Sort

Generic order-by for non-heavy-hitter cases. `partition_by` makes the ordering per-group —
the home for PromQL `topk by (...)`/SQL `... OVER (PARTITION BY ...)`-style grouped ranking.

```promql
sort_desc(up)
```

### Limit

Caps the row count, with an offset. SQL `LIMIT n [OFFSET o]`; also paired with `Sort` for
PromQL's generic (non-heavy-hitter) `topk`/`bottomk`.

```promql
topk(3, up)
```

### Distinct

δ — row-level deduplication (SQL `SELECT DISTINCT`). Distinct from `AggIntent::Cardinality`
(`COUNT(DISTINCT col)`), which collapses to a single number — `Distinct` still returns
multiple rows.

```sql
SELECT DISTINCT srcip, dstip FROM packets
```

### Join

Logical join; the physical strategy (hash/merge/broadcast) is picked at L4. SQL `JOIN`.

```sql
SELECT u.prefix FROM bgp_updates u JOIN bgp_rib_state r ON u.prefix = r.prefix
```

### SetOp

SQL's typed set operations — `UNION`/`INTERSECT`/`EXCEPT` — as opposed to `Merge`'s untyped
concatenation. `UNION` (dedup) further wraps a `SetOp` in a `Distinct`; `UNION ALL` does not.

```sql
SELECT srcip FROM packets UNION ALL SELECT dstip FROM packets
```

### Merge

⊕ — exact, n-ary `UNION ALL` of independent, union-compatible branches; rows concatenate,
never dedup. Used when a single `Aggregate` can't express the shape — the canonical case is
PromQL `histogram_quantiles` (one branch per φ, each its own `HistogramQuantile` reduction
relabeled with its `le` value) — and SQL `ROLLUP`/`CUBE`/`GROUPING SETS` (one branch per
grouping level).

```promql
histogram_quantiles(rate(http_request_duration_seconds_bucket[5m]), "le", 0.5, 0.9)
```

## PromQL-specific nodes

### Scalar

A scalar constant leaf — a PromQL number literal, or a folded constant scalar expression.
Appears as a `BinaryOp` operand for `<vector> op <scalar>` thresholds and unit conversions.

```promql
up > 1
```

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

### ScalarFromVector

The instant-vector→scalar bridge — PromQL `scalar(v)`. Collapses a single-element vector to
its value (NaN at runtime if the input isn't exactly one series).

```promql
scalar(up)
```

### Relabel

ρ — a per-series label rewrite. PromQL `label_replace`/`label_join`; every row passes
through unchanged except for the destination label, whose new value is computed from the
child's label columns.

```promql
label_replace(up, "foo", "$1", "bar", "(.*)")
```

### InfoJoin

PromQL `info(v, [selector])` — left-join label enrichment. Each series in the child is
enriched with labels from the matching info metric(s) (`target_info` by default).

```promql
info(up)
```

### Sample

Series-sampling selection — PromQL `limitk`/`limit_ratio`. Keeps a subset of whole series
per group (or globally); not a ranking (`TopK`) and not a reduction, since the output schema
equals the child's.

```promql
limitk(3, up)
```

## SQL-specific nodes

### WindowFunc

SQL analytic window function: `func(args) OVER (PARTITION BY ... ORDER BY ...)`. Output
schema is the child schema plus one new column for the window expression's result.

```sql
SELECT srcip, LAG(time) OVER (PARTITION BY srcip ORDER BY time) FROM packets
```
