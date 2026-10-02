# Pre-ASAP IR

This is the detailed node reference. Start with the [Pre-ASAP IR concept](../design_docs/concepts/pre-asap-ir.md) for purpose and the compact catalog.

ASAPPlanner has **one operator IR before and after ASAP optimization**, defined in
`crates/types/src/ir/`. "Pre-ASAP" is not a separate type: it is this IR as a front end
emits it, before any ASAP operator has been introduced. This document covers what every
plan shares — the node, the schema, scalar expressions, how front ends produce the DAG, and
the catalog of ordinary (`NonASAPOp`) operators. The ASAP operators, execution timing and
the exported wire form are described in the [Post-ASAP IR](../design_docs/concepts/post-asap-ir.md)
document; the two do not repeat each other.

The goal of the pre-ASAP form is to represent operations from different query languages in a single representation, and make it easier to analyze how/where ASAP primitives can be used.
Only operations that are semantically relevant to answering the query and selecting an ASAP primitive need to become first-class nodes here.

## Design principles

1. Expose the query semantics that affect summary applicability, correctness, and cost.
2. If an operation changes presentation but does not change the semantic summary intent, it does not need to be represented.
3. Equivalent SQL, PromQL, and future-language queries should produce the same intent shape.

> Notes: **SQL and PromQL use different schema models**. SQL typically uses a closed schema, where tables, columns, and types are predefined, while PromQL uses an open (schemaless) schema, where metrics and labels can evolve without a fixed table schema. Closed schemas provide stronger structure and validation; open schemas provide greater flexibility and makes it easier to evolve or ingest diverse data, but can require more care around naming conventions, label cardinality, and query consistency.

## The node

A plan is a DAG of `Rc<OperatorNode>` (`crates/types/src/ir/node.rs`). Nodes are immutable
and shared through `Rc`: a structurally identical sub-DAG referenced from several parents is
one node, and that pointer identity is what CSE, target discovery and plan assembly key on.

```rust
pub struct OperatorNode {
    pub operator: Operator,                   // NonASAP(NonASAPOp) | ASAP(ASAPOp)
    pub result_kind: OperatorResultKind,      // Relation | InstantVector | RangeVector | State | Scalar
    pub schema: Schema,                       // output schema, derived at construction
    pub guarantee: Option<ResultGuarantee>,   // None until accuracy assessment establishes one
    pub timing: Option<ExecutionTiming>,      // None until a lifecycle assignment is applied
}
```

- `operator` is the operation. A front-end DAG contains only `Operator::NonASAP` nodes;
  `OperatorNode::expect_non_asap()` relies on that.
- `result_kind` is the output category, derived from the operator and its inputs. Matching
  column schemas do not make categories interchangeable (a range vector is not an instant
  vector).
- `schema` is derived by `OperatorNode::new(operator)`; it fails when the schema cannot be
  derived (a column reference out of range, a reserved ASAP operator). ASAP planning may
  retain a more specific schema through `OperatorNode::with_schema`.
- `guarantee` is `None` until accuracy assessment establishes one; `None` never means exact.
- `timing` is `None` in every front-end DAG and every candidate. It is written by
  `ir::timing::apply_lifecycle_timings` (see the Post-ASAP IR document); export rejects an
  untimed node.

`OperatorNode::children()` returns the operator's inputs in field order followed by the
operator nodes its scalar expressions read (see "Scalar expressions"). Every DAG traversal —
`map_children`, `reachable`, `contains_asap`, CSE, export — follows that same list.
`OperatorNode::validate_structure()` checks every operator's input contract, scalar typing
against the owning operator's input schema, and that each retained schema agrees with the
derived one.

## Schema

One `Schema` type (`crates/types/src/pre_asap/schema.rs`) describes every edge, whether it
carries rows or summary state:

```rust
pub struct Schema {
    pub fields: Vec<Field>,            // positional; every ColumnId indexes into this
    pub time_index: Option<ColumnId>,  // the time axis, if any (PromQL leaves always have one)
    pub unique_keys: Vec<Vec<ColumnId>>,
    pub closed: bool,                  // true: these are all the columns; false: open (schemaless) superset
}

pub struct Field {
    pub name: String,
    pub dtype: FieldDataType,          // Plain(DataType) | ExactAggregate(..) | Sketch(..) | Sample(..) | Wavelet(..) | StatModel(..)
    pub nullable: bool,
    pub table: Option<String>,         // SQL table/alias qualifier; None for PromQL labels
}
```

A pre-ASAP field is always `FieldDataType::Plain(DataType)`. The other variants carry summary
state and only appear below an ASAP operator; a scalar expression that reads such a field is a
typing error (`ScalarExpr::scalar_type`), because state must be read out before a value can use
it. Column references are positional `ColumnId`s (indexes into the input schema), never names.

`Schema::has_unique_key()` is the legality gate CSE uses: a non-ASAP producer is only shared
across consumers when its row identity is provable.

## Scalar expressions

Value computation lives in `ScalarExpr` (`crates/types/src/ir/scalar.rs`), owned **by value**
by an operator field: `Scan.predicates`, `Filter.pred`, `Join.pred`, `Project.cols[i].expr`,
`Aggregate.having`, `Sort.keys[i].expr`, `SQLWindowFunc.args`/`order_by`, `PromqlRelabel.value`,
`Values.rows`, and `QueryRoot::Scalar` and `PromqlVectorFromScalar`. A scalar expression never
produces a table and is never a node of the DAG; it is evaluated against the input schema of
the operator that owns it.

Variants: `Column(ColumnId)`, `Literal(ScalarValue)`, `Negative` (unary minus), `Compare`,
`BoolAnd` / `BoolOr` (flat conjunction/disjunction), `Not`, `IsNull` / `IsNotNull`, `Cast`
(with `try_cast`), `InList`, `FunctionCall { name, args }`, `Arithmetic`, `Case`,
`CurrentTimestamp` (SQL `NOW()`), `EvalTimestamp` (PromQL `time()`), and four
**plan-reading** variants that reference an operator node:

| Variant | Meaning |
|---|---|
| `PromqlScalarFromVector(Rc<OperatorNode>)` | PromQL `scalar(v)`: the single sample of an instant vector, NaN otherwise |
| `ScalarSubquery(Rc<OperatorNode>)` | Uncorrelated SQL scalar subquery: one column; zero rows is NULL, more than one row is an error |
| `Exists { subquery, negated }` | SQL `[NOT] EXISTS (subquery)` |
| `InSubquery { expr, subquery, negated }` | SQL `expr [NOT] IN (subquery)` over a one-column relation |

These are the **only** operator references inside a scalar tree. `ScalarExpr::operator_refs()`
lists them, `NonASAPOp::children()` appends them after the operator's own inputs, and
canonicalization lowers the three SQL subquery forms to joins (see below), so a canonical SQL
DAG contains none of them. `PromqlScalarFromVector` survives canonicalization: its referenced
vector is a real plan dependency, exported as a `ScalarRef` edge.

`Compare`, `Arithmetic` and `Negative` carry an `ExprSemantics` (`Sql` or `Promql`): both
languages use `Float64`, so the result type alone does not preserve NaN, ordering or error
rules, and the executing engine needs to know which language's rules apply.

Wrapper types: `Predicate(ScalarExpr)`, `ProjectItem { alias, expr }`,
`SortKey { expr, ascending, nulls_first }`.

## How a front end produces the DAG

A front end never constructs `OperatorNode`s directly. It builds a name-based tree in
`crates/frontend-common` — `UnresolvedOp` / `UnresolvedScalar`, a mirror of `NonASAPOp` /
`ScalarExpr` in which every column reference is a `ColumnRef` and a PromQL `Scan` has no schema
yet — and calls `asap_frontend_common::resolve_root`, which does three things in order:

1. **Resolution** — a bottom-up walk that binds every `ColumnRef` to a positional `ColumnId`
   against the derived schema of the already-resolved child. A schemaless (PromQL) leaf gets
   its binding schema from `SchemaResolver`, built from the names the query references.
   `Join` / `SetOp` sides and the operators referenced from scalar positions are each bound as
   a root in their own scope; a `BinaryOp` side additionally inherits the label names its
   enclosing scope references.
2. **Schema derivation** — each `OperatorNode::new` derives the node's output schema and
   result kind from the operator and its children.
3. **Canonicalization** — `asap_types::ir::canonicalize::canonicalize` erases structural
   differences between semantically identical queries: it promotes an additive
   `Limit { Sort { Aggregate } }` ranking to the `AggIntent::TopK` heavy-hitter shape, and
   lowers `EXISTS` / `NOT EXISTS` / `IN (subquery)` predicates to `Join { Semi | Anti }` and a
   scalar subquery to a `Join { Cross }` plus column reference. The pass is idempotent and
   keeps the pointer identity of every untouched sub-DAG.

The result is `Rc<OperatorNode>`. `lower_promql_workload`, `lower_sql` / `lower_sql_dialect` /
`lower_sql_batch` and `lower_metricsql` all return it.

Workload search then runs structural CSE (`asap_types::ir::cse::share_common_sub_dags`) once
across every root: bottom-up hash-consing where the structural hash is only a filter and the
typed `PartialEq` decides sharing, following scalar references like any other input, and
gated by `Schema::has_unique_key()` for non-ASAP producers.

## Fields and column references

`Schema` owns `Field` metadata: name, type, nullability, and an optional table
qualifier. A `Field` contains no runtime values. The former schema `Column`
struct served this same metadata role; it was renamed to `Field`, not retained
as a second data container.

`ColumnRef` is an unresolved logical reference (`Named`, `Qualified`,
`SampleValue`, or `Wildcard`). Resolution binds a reference to `ColumnId`, a
`usize` position within a particular schema. `ScalarExpr::Column(ColumnId)`
reads that column; the same position indexes `Schema::fields` for type checking
and a runtime row for its value. Group keys, unique keys, and `time_index` also
use these column positions. They are not stable identities across projections
or joins, so the positional reference remains `ColumnId`, not `FieldId`.

The native runtime names shared ownership `SchemaRef = Arc<Schema>` and stores
`Batch { schema: SchemaRef, rows: Vec<Vec<Value>> }`. `Schema` is the same metadata
model during planning and execution; the `Ref` suffix only distinguishes ownership.
It has no physical `Column`/array container. A column reference expresses what
to read independently of whether an executor stores its data as rows or arrays.
For example, resolving `t.bytes` to `ColumnId = 1` obtains its type from
`schema.fields[1]`; native execution reads `row[1]`.

## Node index

Grouped to match the sections below — common relational nodes first, then the nodes specific
to one source language.

**[Aggregation-related nodes](#aggregation-related-nodes)**
- [`Aggregate`](#aggregate) — collapses input rows into fewer output rows via a reduction and aggregate intents.

**[Time-related nodes](#time-related-nodes)**
- [`TimeRange`](#timerange) — temporal selection over a time-series input (PromQL instant lookback or `[5m]` range selector).
- [`TimeShift`](#timeshift) — shifts *when* a selector is evaluated (PromQL `offset`/`@`).
- [`PromqlSubquery`](#promqlsubquery) — re-evaluates an instant-vector expression over a range at a given step.

**[Relational nodes](#relational-nodes)** — common to both SQL and PromQL
- [`Scan`](#scan) — identifies the logical data source.
- [`Values`](#values) — SQL `VALUES` rows, or the one empty row of a `SELECT` without `FROM`.
- [`Filter`](#filter) — restricts rows using a predicate.
- [`Project`](#project) — column projection (SQL `SELECT` list).
- [`BinaryOp`](#binaryop) — arithmetic / comparison / set composition of two inputs.
- [`Sort`](#sort) — generic (non-heavy-hitter) order-by, optionally per-group.
- [`Limit`](#limit) — caps the row count, with an offset, optionally per-group.
- [`Dedup`](#dedup) — row-level deduplication.
- [`Join`](#join) — logical join of two inputs.
- [`SetOp`](#setop) — SQL's typed set operations (`UNION`/`INTERSECT`/`EXCEPT`).
- [`Concat`](#concat) — exact, untyped `UNION ALL` of union-compatible branches.

**[Scalar-position nodes](#scalar-position-nodes)**
- `QueryRoot::Scalar` — a standalone scalar expression, without an operator node.
- [`PromqlVectorFromScalar`](#promqlvectorfromscalar) — promotes a scalar to a label-less instant vector.

**[PromQL-specific nodes](#promql-specific-nodes)**
- [`PromqlRelabel`](#promqlrelabel) — per-series label rewrite (PromQL `label_replace`/`label_join`).
- [`PromqlInfoEnrich`](#promqlinfoenrich) — left-join label enrichment from an info metric.
- [`PromqlSeriesSample`](#promqlseriessample) — keeps a subset of whole series, not a reduction.

**[SQL-specific nodes](#sql-specific-nodes)**
- [`SQLWindowFunc`](#sqlwindowfunc) — SQL analytic window function (`OVER (...)`).

PromQL `time()` and `scalar(v)` are scalar expressions (`ScalarExpr::EvalTimestamp`,
`ScalarExpr::PromqlScalarFromVector`), not nodes.

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
      child = TimeRange(range = 5m, kind = Range, child = Scan("http_requests_total"))
  )
  ```

#### Measures

Each entry in `measures` names one statistic to compute. The vocabulary is wider than a minimal
aggregate algebra needs, because it also covers PromQL's range-vector functions and
native-histogram accessors — this list is representative, not exhaustive:

```text
Count, Sum(col), Min(col), Max(col), Avg(col), StdDev(col), Variance(col),
Quantile(col, q), TopK(k), Cardinality(cols), PearsonCorr(left, right)  // data-model-agnostic
Rate, Increase                                                    // counter derivatives
Changes, Delta, IDelta, Deriv, Resets,
PredictLinear(seconds), DoubleExpSmoothing(sf, tf)                // range-vector functions
HistogramCount, HistogramSum, HistogramAvg, HistogramStdDev,
HistogramStdVar, HistogramFraction(lo, hi)                       // native-histogram accessors
HistogramQuantile(q, le)                                          // classic-bucket quantile
Math(func)                                                        // element-wise transform
```

`HistogramQuantile { q, le }` names its bucket-bound column `le`. PromQL's
`Aggregate` groups it `without([le])`, so one histogram is the set of series
that differ only in `le`. The SQL `asap_histogram_quantile` bridge reads one
histogram over all rows.

`PearsonCorr { left, right }` has two value inputs. Both
references resolve to positional column IDs, and `input_cols()` exposes both
dependencies. SQL lowering projects both arguments, preserving
expressions, casts, and qualified join columns. The result is nullable `Float64`,
with pairwise null handling owned by the executing engine. It remains exact:
finalized correlation coefficients cannot be combined as scalar rollups, and no
sketch or maintained correlation accumulator is selected. Physical costing accepts
it as a hash aggregate with provider-supplied accumulator size.

`Cardinality { cols, accuracy }` carries a list, not one column. One entry is
SQL `COUNT(DISTINCT col)`; several count distinct *tuples*
(`COUNT(DISTINCT a, b)`), which is not the distinct count of any one of them.
Empty is the PromQL convention "the sample value" (`count_values`,
`distinct_over_time`). Serialized intents reject unknown fields: legacy
`Cardinality` payloads containing `col` must be migrated to `cols` before loading
(`col: n` becomes `cols: [n]`, and `col: null` becomes `cols: []`). Omitting both
fields still selects the implicit sample input.

`input_cols()` is the only column accessor on `AggIntent` — an intent's arity is
its own business, so no consumer can ask for "the" input column of an aggregate
that reads two and silently receive one leg of it. That mattered concretely:
before `Cardinality` took a list, SQL lowering dropped every argument after the
first, reporting single-column cardinality as tuple cardinality.

Realization is the single-column one with a wider item: a tuple becomes a
`SummaryInputExpr::Tuple`, which the distinct-count sketches (HLL, Theta, KMV)
hash as one value. UnivMon is withheld from a tuple — it estimates frequency
moments over a single value stream. At `AccuracyTarget::Exact` the node stays a
logical pass-through at any arity. Each SQL argument must be a bare column,
qualifier preserved so a tuple over a join resolves to the correct side; an
expression argument is rejected rather than reduced over a probe column.

SQL `corr` currently rejects `DISTINCT`, aggregate `ORDER BY`,
explicit null treatment, and window usage (`OVER`); a `FILTER` clause becomes the
measure's own predicate (see `filters` below). Further two-input statistics
(`covar`, the `regr_*` family) would each add their own variant, following the
`StdDev` / `Variance` precedent, once they have explicit output and realization
semantics.

Additional measures can be added when there is a stable semantic distinction and a
meaningful summary implementation.

**Fields:**
- `reduction` — how rows are grouped/collapsed; see "Reduction" above.
- `measures` — the aggregate intents to compute; see "Measures" above.
- `output_names` — output column name per entry in `measures`; a non-empty entry overrides the
  synthetic default — SQL threads DataFusion's generated name (e.g. `"sum(metrics.bytes)"`)
  here so an enclosing `Project` can resolve the aggregate output by the name it references.
- `filters` — one optional row predicate per entry in `measures`, with SQL
  `FILTER (WHERE …)` semantics (#466): only rows where `filters[i]` is `TRUE` update
  `measures[i]`; groups are still formed from every row. It is positional against
  `child`'s output (like `Filter.pred`), not against the aggregate's output like `having`.
  Empty means no measure is filtered; that is the only spelling of "unfiltered" a resolved
  DAG carries, so `[None, None]` is normalized to `[]`.
- `having` — an optional post-aggregation filter predicate (SQL `HAVING`).
- `child` — the input being aggregated.

Example for `filters`:

  ```sql
  SELECT l_shipmode, count(CASE WHEN l_returnflag = 'R' THEN 1 END), sum(l_quantity)
  FROM lineitem GROUP BY l_shipmode
  ```

  is one scan and one grouping, so it is one `Aggregate`. The conditional count is a plain
  `Count` whose filter is the `CASE` condition; the sum is unfiltered:

  ```text
  Aggregate(
      reduction = Reduce(by = [l_shipmode]),
      measures = [Count, Sum(l_quantity)],
      filters = [Some(l_returnflag = 'R'), None],
      child = Scan("lineitem"),
  )
  ```

  The SQL front end fills `filters` from an explicit `FILTER (WHERE p)`, from
  `count(CASE WHEN p THEN x END)` (`p`, plus `x IS NOT NULL` when `x` is nullable), and from
  `count(expr)` over any other nullable `expr` (`expr IS NOT NULL`), because canonical `Count`
  counts rows and never consults its argument. A filtered measure has no summary binding yet:
  `asap-aware-mapping` retains such an `Aggregate` as an ordinary exact sub-DAG, and canonicalization does
  not promote a filtered count ranking to a heavy-hitter `TopK`.

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
   the node it actually applies to, not behind an opaque wrapper, once summary binding reads it.
   The upper nodes (closer to the root) in the AST/DAG can still have a `Filter` node with the same condition. This intentional duplication is for Summary related translation and optimizations.

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

   but the DAG can still carry the same condition as a wrapping `Filter` near the root:

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
   once HAVING-aware summary binding lands.

  Neither direction of that push-down is enforced yet: the SQL front end doesn't populate
  `having` from a real `HAVING` clause (#201), and canonicalization doesn't fold an existing
  `Filter { child: Aggregate { having: None, .. } }` into `Aggregate { having: Some(..), .. }`
  either (#204).


## Time-related nodes

### TimeRange

Temporal selection over a time-series input. Kept different from `Filter` to treat time as an
explicit concern. `kind` records which samples a PromQL selector reads:

- `TimeRangeKind::Instant` — an instant selector: `range` is the lookback horizon and the
  latest eligible sample per series is selected (the planner injects the declared
  `data_ingestion_interval` around a bare selector).
- `TimeRangeKind::Range` — a range selector (`m[5m]`): every sample in the window.

```promql
rate(http_requests_total[5m])
```

**Fields:**
- `range` — how far back to look (the PromQL `[5m]` duration, or the instant lookback).
- `kind` — `Instant` or `Range`.
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
  rule 1): PromQL label matchers and pushed-down `WHERE` conjuncts.
- `schema` — the binding schema every positional column reference in the tree resolves against.
  A catalog-backed SQL leaf carries its catalog schema; a PromQL leaf carries the usage-derived
  schema `SchemaResolver` built from the labels the query references.

### Values

SQL `VALUES` rows, or the one empty row of a `SELECT` without `FROM`
(`SELECT 1 + 1`). Row expressions have no input-column scope.

**Fields:**
- `rows` — one `Vec<ScalarExpr>` per row.
- `schema` — the output schema of the rows.

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

A `Filter` whose predicate contains `EXISTS` / `NOT EXISTS` / `IN (subquery)` does not
survive canonicalization: the conjunct becomes a `Join { Semi | Anti }` under the remaining
predicate.

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

Arithmetic / comparison / set composition of two operands. PromQL binary operators between two vectors,
two vectors. Mixed vector/scalar arithmetic uses `Project`; non-bool comparison uses `Filter`. Standalone scalar expressions are `QueryRoot::Scalar`.

```promql
up > 1
```

**Fields:**
- `operator` — a `BinaryOperator { kind, vector_match, checked_relative_division, checked_finite_division }`:
  - `kind` — `BinaryOpKind::Arithmetic(..)`, `Compare(..)` or `Set(..)` (PromQL `and`/`or`/`unless`).
  - `vector_match` — PromQL vector-matching modifiers (`on`/`ignoring`, `group_left`/`group_right`); `None` outside PromQL and the only supported value today.
  - `checked_relative_division` / `checked_finite_division` — typed division guards set by summary planning, never by a front end (see [physical-plan integration](../design_docs/architecture/physical-plan-integration.md#conditional-temporal-average-lowering)).
- `return_bool` — the PromQL `bool` modifier: a comparison returns `0`/`1` instead of filtering. Valid only for comparison operators.
- `lhs` — the left operand.
- `rhs` — the right operand.

### Sort

Generic order-by for non-heavy-hitter cases. `partition_by` makes the ordering per-group —
the home for PromQL `topk by (...)`/SQL `... OVER (PARTITION BY ...)`-style grouped ranking.

```promql
sort_desc(up)
```

**Fields:**
- `keys` — the ordering expressions and direction (`SortKey`).
- `partition_by` — grouping keys that make the ordering per-group instead of global; empty = a single global order.
- `child` — the input being ordered.

### Limit

Caps the row count, with an offset. SQL `LIMIT n [OFFSET o]`; also paired with `Sort` for
PromQL's generic (non-heavy-hitter) `topk`/`bottomk`.

```promql
topk(3, up)
```

**Fields:**
- `n` — the maximum number of rows to keep; `None` is offset-only.
- `offset` — how many leading rows to skip first.
- `partition_by` — applies the limit per group (PromQL `topk by (..)`); empty = global.
- `child` — the input being capped.

### Dedup

δ — row-level deduplication (SQL `SELECT DISTINCT`). Distinct from
`AggIntent::Cardinality` (`COUNT(DISTINCT col)`, `COUNT(DISTINCT a, b)`), which
collapses to a single number — `Dedup` still returns multiple rows.

```sql
SELECT DISTINCT srcip, dstip FROM packets
```

**Fields:**
- `cols` — the columns to dedup on; empty = dedup on every column.
- `child` — the input being deduplicated.

### Join

Logical join; the physical strategy (hash/merge/broadcast) is picked downstream of the planner. SQL `JOIN`,
and the shape canonicalization lowers subqueries to.

```sql
SELECT u.prefix FROM bgp_updates u JOIN bgp_rib_state r ON u.prefix = r.prefix
```

**Fields:**
- `kind` — the join type (`Inner`/`Left`/`Right`/`Full`/`Cross`/`Semi`/`Anti`). A semi/anti join
  outputs the left input's columns alone, but its predicate resolves against `left ++ right`.
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
grouping level). The output schema is the first child's.

```promql
histogram_quantiles(rate(http_request_duration_seconds_bucket[5m]), "le", 0.5, 0.9)
```

**Fields:**
- `children` — the union-compatible branches to concatenate; must be non-empty.
- `discriminator_unique_key` — an optional caller-proven compound unique key
  `(discriminator, inner_key)` over the output; nothing verifies the claim.

## Scalar-position nodes

### Scalar query roots

`QueryRoot` distinguishes an operator result from an owned `ScalarExpr`. It is
an API root discriminator, not an operator. `2`, `time()`, and
`scalar(sum(up)) + 1` therefore introduce no constant-wrapper nodes.

Use `lower_promql_query_workload` for mixed scalar/vector workloads. The
operator-only convenience API rejects standalone scalar roots. `ParsedWorkload`
retains each scalar's workload index; `PlanOutput::roots()` returns all results
in workload order. Scalar plan reads remain exact and retain their operator
references; summary selection currently operates on operator roots.

`up * 2` projects the sample expression while retaining time and full series
identity, removing the metric name. `up > 0` and `0 < up` filter the vector and
retain its sample and name. `up > bool 0` projects a zero-or-one `Case`.
Open label schemas acquire a full runtime series-identity field before this
lowering. The runtime must populate that field with all labels.

### PromqlVectorFromScalar

The scalar→instant-vector bridge — PromQL `vector(s)`. Promotes a scalar expression to a
single label-less series carrying that value at every step, e.g. for dead-man's-switch
patterns (`up or vector(0)`).

```promql
vector(1)
```

**Fields:** a single unnamed `ScalarExpr` — the scalar being promoted to a vector.

## PromQL-specific nodes

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
- `frame` — the optional window frame.
- `output_name` — the name of the new output column.
- `child` — the input the window function is computed over.
