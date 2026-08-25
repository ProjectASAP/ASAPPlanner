# Pre-ASAP IR

The goal of the pre-ASAP IR is represent operations from different query languages in a single representation, and make it easier to analyze how/where ASAP primitives can be used.
Only operations that are semantically relevant to answering the query and selecting an ASAP primitive need to become first-class nodes here.

## Design principles

1. Expose the query semantics that affect summary applicability, correctness, and cost.
2. If an operation changes presentation but does not change the semantic summary intent, it does not need to be represented.
3. Equivalent SQL, PromQL, and future-language queries should produce the same intent shape.

> Notes: **SQL and PromQL use different schema models**. SQL typically uses a closed schema, where tables, columns, and types are predefined, while PromQL uses an open (schemaless) schema, where metrics and labels can evolve without a fixed table schema. Closed schemas provide stronger structure and validation; open schemas provide greater flexibility and makes it easier to evolve or ingest diverse data, but can require more care around naming conventions, label cardinality, and query consistency.

The pre-ASAP IR is defined using the `QueryExpr` enum. We discuss some of important enum types below.

## Node index

Grouped to match the sections below — common relational nodes first, then the nodes specific
to one source language.

**[Aggregation-related nodes](#aggregation-related-nodes)**
- [`Aggregate`](#aggregate) — collapses input rows into fewer output rows via a reduction and aggregate intents.

**[Time-related nodes](#time-related-nodes)**
- [`TimeRange`](#timerange) — a range-vector lookback over the time axis (PromQL `[5m]`).
- [`TimeShift`](#timeshift) — shifts *when* a selector is evaluated (PromQL `offset`/`@`).
- [`PromqlSubquery`](#promqlsubquery) — re-evaluates an instant-vector expression over a range at a given step.

**[Relational nodes](#relational-nodes)** — common to both SQL and PromQL
- [`Scan`](#scan) — identifies the logical data source.
- [`Filter`](#filter) — restricts rows using a predicate.
- [`Project`](#project) — column projection (SQL `SELECT` list).
- [`BinaryOp`](#binaryop) — arithmetic / comparison / boolean composition of two inputs.
- [`Sort`](#sort) — generic (non-heavy-hitter) order-by, optionally per-group.
- [`Limit`](#limit) — caps the row count, with an offset.
- [`Dedup`](#dedup) — row-level deduplication.
- [`Join`](#join) — logical join of two inputs.
- [`SetOp`](#setop) — SQL's typed set operations (`UNION`/`INTERSECT`/`EXCEPT`).
- [`Concat`](#concat) — exact, untyped `UNION ALL` of union-compatible branches.

**[PromQL-specific nodes](#promql-specific-nodes)**
- [`PromqlScalarBridge`](#promqlscalarbridge) — a scalar sub-expression at an operator-tree position.
- [`EvalTimestamp`](#evaltimestamp) — the query evaluation time as a scalar (PromQL `time()`).
- [`PromqlVectorFromScalar`](#promqlvectorfromscalar) — promotes a scalar to a label-less instant vector.
- [`PromqlScalarFromVector`](#promqlscalarfromvector) — collapses a single-series vector to a scalar.
- [`PromqlRelabel`](#promqlrelabel) — per-series label rewrite (PromQL `label_replace`/`label_join`).
- [`PromqlInfoEnrich`](#promqlinfoenrich) — left-join label enrichment from an info metric.
- [`PromqlSeriesSample`](#promqlseriessample) — keeps a subset of whole series, not a reduction.

**[SQL-specific nodes](#sql-specific-nodes)**
- [`SQLWindowFunc`](#sqlwindowfunc) — SQL analytic window function (`OVER (...)`).

## Aggregation-related nodes

### Aggregate

Collapses input rows into fewer output rows via a `reduction` plus a
list of aggregate intents (`measures`).

#### Reduction

`Aggregate.reduction` is a `Reduction` — richer than a plain SQL `GROUP BY` — one of:

- **`Reduce(GroupKeys)`** — a cross-row reduction: group by some columns, or group
  by every column *except* some listed ones. `GroupKeys` holds positional column references
  (`ColumnId`s — indexes into the input schema, not column names) and carries a `by`/`without`
  flag, not just a plain list:
  - `by(keys)` — group by exactly these columns (SQL `GROUP BY`, PromQL `by(...)`).
  - `without(keys)` — group by every column *except* these (PromQL `without(...)`); the
    excluded columns are stored but the full set of all columns stay open (because PromQL is schemaless), resolved at runtime against
    the actual input schema.
  - `none()` — an empty GroupKeys list, i.e. a global (ungrouped) reduction. This is different from `PerEntity` below.

  ```text
  Aggregate(
      reduction = Reduce(by = [service, region]),   // GROUP BY service, region
      measures = [Count],
      output_names = [],
      having = None,
      child = ...
  )
  ```
- **`PerEntity`** — no collapsing multiple input rows/entities into fewer output rows: each input entity keeps its own output row (the
  value is still recomputed by the agg intent, e.g. `Rate`), for a computation with no
  `by(...)` clause to attach to. `PerEntity` is different from `by` for all columns, because in PromQL, it is schemaless and you don't know all columns beforehand.
   E.g. PromQL `rate(http_requests_total[5m])`, which has one rate value
  per input series:

  ```text
  Aggregate(
      reduction = PerEntity,
      measures = [Rate],
      output_names = [],
      having = None,
      child = TimeRange(range = 5m, child = Scan("http_requests_total"))
  )
  ```

#### Measures

Each entry in `measures` names one statistic to compute. The vocabulary is wider than a minimal
aggregate algebra needs, because it also covers PromQL's range-vector functions and
native-histogram accessors — this list is representative, not exhaustive:

```text
Count, Sum(col), Min(col), Max(col), Avg(col), StdDev(col), Variance(col),
Quantile(col, q), TopK(k), Cardinality(col)                      // data-model-agnostic
Rate, Increase                                                    // counter derivatives
Changes, Delta, IDelta, Deriv, Resets,
PredictLinear(seconds), DoubleExpSmoothing(sf, tf)                // range-vector functions
HistogramCount, HistogramSum, HistogramAvg, HistogramStdDev,
HistogramStdVar, HistogramFraction(lo, hi), HistogramQuantile(q)  // native-histogram accessors
Math(func)                                                        // element-wise transform
```

Additional measures can be added when there is a stable semantic distinction and a
meaningful summary implementation.

**Fields:**
- `reduction` — how rows are grouped/collapsed; see "Reduction" above.
- `measures` — the aggregate intents to compute; see "Measures" above.
- `output_names` — output column name per entry in `measures`; a non-empty entry overrides the
  synthetic default — SQL threads DataFusion's generated name (e.g. `"sum(metrics.bytes)"`)
  here so an enclosing `Project` can resolve the aggregate output by the name it references.
- `having` — an optional post-aggregation filter predicate (SQL `HAVING`).
- `child` — the input being aggregated.

Example for `having`:

  ```sql
  SELECT srcip, COUNT(*) AS cnt FROM packets GROUP BY srcip HAVING COUNT(*) > 10
  ```

  What the *field* is for is putting the `cnt > 10` predicate directly on the `Aggregate` that
  produces `cnt`, instead of behind a separate `Filter`:

  ```text
  Aggregate(
      reduction = Reduce(by = [srcip]),
      measures = [Count],
      output_names = ["cnt"],
      having = Some(cnt > 10),
      child = Scan("packets"),
  )
  ```

**Rules/Invariants**: A filtering predicate will be passed to at the lowest node (closer to the leaves) in the AST/DAG that can express it — `Scan.predicates`,
   then `Aggregate.having`, then `Filter` as the fallback — so its constraint is visible at
   the node it actually applies to, not behind an opaque wrapper, once pre-ASAP IR translates
   to post-ASAP IR with summary binding. The upper nodes (closer to the root) in the AST/DAG can still have a `Filter` node with the same condition. This intentional duplication is for Summary related translation and optimizations.

   For example, `SELECT srcip, COUNT(*) AS cnt FROM packets GROUP BY srcip HAVING COUNT(*) > 10`
   pins `cnt > 10` to the lowest node that can express it, `Aggregate.having`:

   ```text
   Aggregate(
       reduction = Reduce(by = [srcip]),
       measures = [Count],
       output_names = ["cnt"],
       having = Some(cnt > 10),
       child = Scan("packets"),
   )
   ```

   but the tree can still carry the same condition as a wrapping `Filter` near the root:

   ```text
   Filter(
       pred = cnt > 10,
       child = Aggregate(
           reduction = Reduce(by = [srcip]),
           measures = [Count],
           output_names = ["cnt"],
           having = Some(cnt > 10),
           child = Scan("packets"),
       ),
   )
   ```

   Both are valid at once, and neither is derived from the other: `having` is the canonical
   spot a summary-aware pass reads to decide whether `Aggregate` can bind to a summary, while
   the outer `Filter` is what a plain logical evaluator runs without knowing `having` exists. The duplication is forward-looking groundwork for
   once HAVING-aware summary binding (pre-ASAP-IR to post-ASAP-IR translation) lands.

  Neither direction of that push-down is enforced yet: the SQL front end doesn't populate
  `having` from a real `HAVING` clause (#201), and canonicalization doesn't fold an existing
  `Filter { child: Aggregate { having: None, .. } }` into `Aggregate { having: Some(..), .. }`
  either (#204).


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

### PromqlSubquery

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
- `predicates` — row-level filters pushed all the way down to this scan (Rules/Invariants
  rule 1); enforced structurally at lowering time — a `Filter` directly over a `Scan` never
  survives.
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

Note (Rules/Invariants rule 1): a predicate pushes down to the lowest node that can express it. A
`WHERE`/label-matcher predicate directly on a base table scan lands in `Scan.predicates`, not
a `Filter` node. A predicate directly over an enclosing `Aggregate`'s own output — SQL
`HAVING`, or an equivalent derived-table `WHERE` — belongs in that `Aggregate`'s `having`
field instead (see `Aggregate`'s `having` field above), not a `Filter`:

```sql
SELECT * FROM (SELECT srcip, COUNT(*) AS cnt FROM packets GROUP BY srcip) t WHERE cnt > 10
```

`Filter` genuinely survives once neither applies — e.g. a predicate over a computed column
that's neither a base scan column nor an aggregate output:

```sql
SELECT * FROM (SELECT srcip, bytes_in + bytes_out AS total FROM packets) t WHERE total > 500
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

### Dedup

δ — row-level deduplication (SQL `SELECT DISTINCT`). Distinct from `AggIntent::Cardinality`
(`COUNT(DISTINCT col)`), which collapses to a single number — `Dedup` still returns
multiple rows.

```sql
SELECT DISTINCT srcip, dstip FROM packets
```

**Fields:**
- `cols` — the columns to dedup on; empty = dedup on every column.
- `child` — the input being deduplicated.

### Join

Logical join; the physical strategy (hash/merge/broadcast) is picked in the post-ASAP IR. SQL `JOIN`.

```sql
SELECT u.prefix FROM bgp_updates u JOIN bgp_rib_state r ON u.prefix = r.prefix
```

**Fields:**
- `kind` — the join type (inner/left/right/full/semi/anti).
- `pred` — the join condition.
- `left` — the left input.
- `right` — the right input.

### SetOp

SQL's typed set operations — `UNION`/`INTERSECT`/`EXCEPT` — as opposed to `Concat`'s untyped
concatenation. `UNION` (dedup) further wraps a `SetOp` in a `Dedup`; `UNION ALL` does not.

```sql
SELECT srcip FROM packets UNION ALL SELECT dstip FROM packets
```

**Fields:**
- `kind` — which set operation (union/intersect/except).
- `all` — whether duplicates are kept (`ALL`) or removed.
- `left` — the left branch.
- `right` — the right branch.

### Concat

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

### PromqlScalarBridge

A scalar sub-expression (issue #220: in practice always `Literal(ScalarValue::Float64(_))` —
a PromQL number literal, or a folded constant scalar expression) sitting at an **operator-tree
position** — a `BinaryOp` operand for `<vector> op <scalar>` thresholds and unit conversions,
a `PromqlVectorFromScalar` child, or a whole query's root. This wrapper is what marks the
position; it no longer duplicates `Literal`'s value the way the old `PromqlScalar(f64)` variant
did.

```promql
up > 1
```

**Fields:** a single unnamed child `QueryExpr` — the wrapped scalar sub-expression.

### EvalTimestamp

The query **evaluation time** as a scalar — PromQL `time()` — and the implicit input of the
no-argument calendar functions (`hour()`, `day_of_week()`, ...).

```promql
time()
```

### PromqlVectorFromScalar

The scalar→instant-vector bridge — PromQL `vector(s)`. Promotes a scalar-typed child to a
single label-less series carrying that value at every step, e.g. for dead-man's-switch
patterns (`up or vector(0)`).

```promql
vector(1)
```

**Fields:** a single unnamed child `QueryExpr` — the scalar-typed expression being promoted to a vector.

### PromqlScalarFromVector

The instant-vector→scalar bridge — PromQL `scalar(v)`. Collapses a single-element vector to
its value (NaN at runtime if the input isn't exactly one series).

```promql
scalar(up)
```

**Fields:** a single unnamed child `QueryExpr` — the single-series vector being collapsed to a scalar.

### PromqlRelabel

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

### PromqlInfoEnrich

PromQL `info(v, [selector])` — left-join label enrichment. Each series in the child is
enriched with labels from the matching info metric(s) (`target_info` by default).

```promql
info(up)
```

**Fields:**
- `selector` — matchers picking which info metric(s) to enrich from; empty = the default `target_info`.
- `child` — the input series being enriched.

### PromqlSeriesSample

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

### SQLWindowFunc

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
