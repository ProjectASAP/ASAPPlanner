# Decoupling Operators From Scalar Expressions

> Status: proposal, not implemented. Audience: planner designers and architects.
> Companion to [Operator sharing](operator-sharing.md).

## 1. Problem and goal

The current query representation mixes query-plan operators and scalar
expressions. Their roles are distinguished by where they occur, so an expression
can be placed where a table input is expected and fail only when the plan is checked.

Separate these concepts so the plan model expresses which combinations are valid.

Consider:

```sql
SELECT l_quantity * 2 AS q2
FROM lineitem
WHERE l_quantity > 10
```

```text
Scan lineitem → Filter → Project
                 │         │
              predicate  expression
              quantity   quantity * 2
                > 10
```

The scan, filter and projection produce tables. The predicate and multiplication
compute values within the schema selected by their owning operators.

## 2. Proposed data structures

`NonASAPOp` describes a relation or vector computation: reading data, selecting
rows or samples, combining inputs, or reducing them. `ScalarExpr` computes one
value in a column and evaluation context. An operator supplies that context;
a scalar query root has no row columns. The distinction is about semantic role,
not whether the source language calls something an “expression”.

Keep existing names where the semantics match. The proposal changes the boundary
where needed, rather than copying every current `QueryExpr` variant unchanged:

| Current representation | Proposed representation | Reason |
|---|---|---|
| `PromqlScalarBridge(Literal(...))` | `ScalarExpr::Literal` | Constants need no operator node. |
| Operator `EvalTimestamp` | Scalar `EvalTimestamp` | `time()` reads evaluation context and returns one number. |
| Operator `PromqlScalarFromVector` | Scalar `PromqlScalarFromVector` referencing its input plan | `scalar(v)` has real cardinality/conversion semantics. |
| Scalar operands wrapped as `BinaryOp` inputs | `Arithmetic` / `Compare` inside `Project` or `Filter` | Vector/scalar operations need no constant-producing input node. |
| `BinaryOp` without comparison mode | Vector/vector `BinaryOp` with `return_bool` | Filtering and numeric comparison results differ. |
| Undifferentiated `TimeRange` | `TimeRange` with instant/range kind | Selecting the latest sample differs from selecting all samples in an interval. |

### 2.1 Operator nodes

The sketches describe the proposed shape, not implemented Rust definitions.
`C` remains `ColumnRef` before resolution and `ColumnId` afterward;
`C::ScanSchema` retains its existing meaning. Operator references are shared,
including `Concat` inputs. Scalar fields own expression trees.

```rust
enum NonASAPOp<C: ColState = ColumnId> {
    Scan {
        source: Source, predicates: Vec<Predicate<C>>, schema: C::ScanSchema,
    },
    Values { rows: Vec<Vec<ScalarExpr<C>>>, schema: C::ScanSchema },
    Filter { pred: Predicate<C>, child: Rc<NonASAPOp<C>> },
    Project {
        cols: Vec<ProjectItem<C>>, qualifier: Option<String>, child: Rc<NonASAPOp<C>>,
    },
    Aggregate {
        reduction: Reduction<C>, measures: Vec<AggIntent<C>>, output_names: Vec<String>,
        having: Option<Predicate<C>>, child: Rc<NonASAPOp<C>>,
    },
    Dedup { cols: Vec<C>, child: Rc<NonASAPOp<C>> },
    Concat {
        children: Vec<Rc<NonASAPOp<C>>>,
        discriminator_unique_key: Option<ConcatDiscriminatorKey<C>>,
    },
    Join {
        kind: JoinKind, pred: Predicate<C>,
        left: Rc<NonASAPOp<C>>, right: Rc<NonASAPOp<C>>,
    },
    SetOp {
        kind: RelationalSetOpKind, all: bool,
        left: Rc<NonASAPOp<C>>, right: Rc<NonASAPOp<C>>,
    },
    Sort { keys: Vec<SortKey<C>>, partition_by: GroupKeys<C>, child: Rc<NonASAPOp<C>> },
    Limit {
        n: Option<usize>, offset: usize, partition_by: GroupKeys<C>,
        child: Rc<NonASAPOp<C>>,
    },
    BinaryOp {
        op: BinaryOpKind, lhs: Rc<NonASAPOp<C>>, rhs: Rc<NonASAPOp<C>>,
        vector_match: Option<VectorMatch>, return_bool: bool,
    },
    SQLWindowFunc {
        func: WindowFuncKind, args: Vec<ScalarExpr<C>>, partition_by: GroupKeys<C>,
        order_by: Vec<SortKey<C>>, frame: Option<WindowFrame>,
        output_name: String, child: Rc<NonASAPOp<C>>,
    },
    TimeRange { range: Duration, kind: TimeRangeKind, child: Rc<NonASAPOp<C>> },
    TimeShift { shift: TimeShift, child: Rc<NonASAPOp<C>> },
    PromqlVectorFromScalar(ScalarExpr<C>),
    PromqlRelabel { dst: String, value: ScalarExpr<C>, child: Rc<NonASAPOp<C>> },
    PromqlInfoEnrich { selector: Vec<InfoMatcher>, child: Rc<NonASAPOp<C>> },
    PromqlSeriesSample { by: GroupKeys<C>, kind: SampleKind, child: Rc<NonASAPOp<C>> },
    PromqlSubquery {
        range: Duration, resolution: Option<Duration>, child: Rc<NonASAPOp<C>>,
    },
}

enum TimeRangeKind { Instant, Range }

struct Predicate<C: ColState = ColumnId>(ScalarExpr<C>);

struct ProjectItem<C: ColState = ColumnId> {
    alias: Option<String>,
    expr: ScalarExpr<C>,
}

struct SortKey<C: ColState = ColumnId> {
    expr: ScalarExpr<C>,
    ascending: bool,
    nulls_first: bool,
}
```

`Predicate`, `ProjectItem` and `SortKey` remain separate: they describe a condition,
a named output and an ordering requirement. `Values` is a relation constructor
for SQL `VALUES` and `SELECT` without `FROM`, not a wrapper for PromQL scalars.
One empty row provides the input for `SELECT 1`; zero rows represent an empty
relation. Its expressions have no input-column scope.

`Limit.n = None` permits offset without a limit. `partition_by` adopts the existing
post-ASAP field for limits within groups, including constant-parameter PromQL
top/bottom selection. `BinaryOp` retains vector matching and gains `return_bool`,
valid only for comparisons. Scalar/vector cases lower as described in §3.3.

`TimeRangeKind::Instant` uses `range` as the lookback horizon and selects the latest
eligible sample per series, respecting staleness. `Range` selects a sample window.
This fixes the current ambiguity between selectors; lookback is not the ingestion
interval. `TimeShift` around a selector or `PromqlSubquery` changes the evaluation
time of that whole input. Existing signed offsets and `AtModifier` anchors remain.

### 2.2 Scalar expressions

Scalar recursion uses owned `Box` and `Vec` children. The only plan references are
explicit operations that consume a query result to compute a value. Those edges
remain visible to plan traversal and costing; they cannot hide a separate plan.
Since these variants reference `NonASAPOp<C>`, `ScalarExpr` and its wrappers retain
`C: ColState`; the earlier proposed removal of that bound no longer applies.

```rust
enum ScalarExpr<C: ColState = ColumnId> {
    Column(C),
    Literal(ScalarValue),
    Negative { expr: Box<ScalarExpr<C>>, semantics: ExprSemantics },
    Compare {
        left: Box<ScalarExpr<C>>, op: CompareOpKind, right: Box<ScalarExpr<C>>,
        semantics: ExprSemantics,
    },
    BoolAnd(Vec<ScalarExpr<C>>),
    BoolOr(Vec<ScalarExpr<C>>),
    Not(Box<ScalarExpr<C>>),
    IsNull(Box<ScalarExpr<C>>),
    IsNotNull(Box<ScalarExpr<C>>),
    Cast { expr: Box<ScalarExpr<C>>, to: DataType, try_cast: bool },
    InList { expr: Box<ScalarExpr<C>>, list: Vec<ScalarExpr<C>>, negated: bool },
    FunctionCall { name: String, args: Vec<ScalarExpr<C>> },
    Arithmetic {
        op: ArithmeticOpKind, left: Box<ScalarExpr<C>>, right: Box<ScalarExpr<C>>,
        semantics: ExprSemantics,
    },
    Case {
        operand: Option<Box<ScalarExpr<C>>>,
        branches: Vec<(ScalarExpr<C>, ScalarExpr<C>)>,
        else_expr: Option<Box<ScalarExpr<C>>>,
    },
    CurrentTimestamp,
    EvalTimestamp,
    PromqlScalarFromVector(Rc<NonASAPOp<C>>),
    ScalarSubquery(Rc<NonASAPOp<C>>),
    Exists { subquery: Rc<NonASAPOp<C>>, negated: bool },
    InSubquery {
        expr: Box<ScalarExpr<C>>, subquery: Rc<NonASAPOp<C>>, negated: bool,
    },
}

enum ExprSemantics { Sql, Promql }
```

`ExprSemantics` distinguishes numeric/comparison rules even when both languages
use `Float64`; a result type alone does not preserve NaN, ordering or error rules.
`Negative` preserves unary negation directly. `Compare` returns Boolean internally;
PromQL numeric comparison results use `Case` to produce `1.0` or `0.0`.
`FunctionCall.name` must resolve to an unambiguous function contract, including
argument/result types, null behavior and volatility. An arbitrary name is not
proof that a function is supported.

The plan-reading variants have different contracts:

| Scalar variant | Required input and result |
|---|---|
| `PromqlScalarFromVector` | An instant vector; one float sample becomes its value, otherwise NaN. |
| `ScalarSubquery` | A one-column SQL relation; zero rows gives typed NULL, one row gives its value, multiple rows is an error. |
| `Exists` | A SQL relation; returns a non-null Boolean based on whether it has any rows. |
| `InSubquery` | A one-column SQL relation; applies SQL membership and NULL rules, including for `NOT IN`. |

These consume query results; they are not interchangeable bridge nodes. The SQL
variants cover uncorrelated subqueries here. Correlation needs outer-scope bindings
that this proposal does not define (§3.4).

### 2.3 Query roots and composition without a bridge

A query root selects one of the two representations; this tag adds no computation:

```rust
enum QueryRoot<C: ColState = ColumnId> {
    Operator(Rc<NonASAPOp<C>>),
    Scalar(ScalarExpr<C>),
}
```

SQL query results use the operator arm. PromQL numeric/string roots use the scalar
arm; vector roots use the operator arm. A scalar root cannot contain free columns.
`EvalTimestamp` reads the current PromQL evaluation time; `CurrentTimestamp` reads
SQL's statement time. Moving `EvalTimestamp` into `ScalarExpr` must not make it
constant across evaluation steps or nested subquery times.

For float samples, the following plans need no `PromqlScalarBridge`:

```text
2                    → Scalar root: Literal(2.0)
time()               → Scalar root: EvalTimestamp
up * 2               → Project(sample * 2.0, child = instant selection of up)
vector(time())       → PromqlVectorFromScalar(EvalTimestamp)
scalar(sum(up)) + 1   → Scalar root: Arithmetic(
                           PromqlScalarFromVector(Aggregate(...)), Literal(1.0))
```

Here, `scalar()` and `vector()` are Prometheus PromQL built-in conversion
functions explicitly present in the query, not wrappers inserted by this proposal.
`sum(up)` alone remains a valid instant-vector query.

A PromQL projection retains the time and label fields required by the operation
and applies its metric-name rules; it does not project only the numeric sample.
For an open label schema, lowering must retain the complete series identity,
including unreferenced labels. If the input provides neither a complete label
schema nor a full identity value, this lowering is not valid.
`vector(s)` remains a real conversion to a one-element, label-free vector.
When combined with the [operator-sharing proposal](operator-sharing.md), all plan
references above target the common `Operator`, including references inside scalar
expressions and query roots. Scalar expression trees do not acquire shared-node
identity.

## 3. Semantic requirements

### 3.1 Version and coverage contract

This section maps source semantics to §2. **Direct** means the structure records
the operation; **Lowered** means an equivalent composition is specified. Neither
means implemented or runtime-verified. **Partial** limits coverage to individually
registered function contracts. **Gap** means §2 lacks a necessary payload,
type or binding rule; the frontend must reject that case until it is supplied.

| Language | Version used for this proposal | Repository relationship |
|---|---|---|
| DataFusion SQL | **DataFusion 55.1.0**, its SQL query dialect and logical expressions | Target semantic baseline. The repository still uses 43.0.0; dependency migration is separate. This is not a claim about every SQL standard or dialect. |
| PromQL | **Prometheus 3.15.0**, with experimental features identified separately | Target semantic baseline. The repository pins ProjectASAP parser revision `9fede7eecca923c9882fe256484d00d37f8706cb`, declaring 3.8 compatibility; upgrading that parser is separate. |

References are pinned to those versions:
[DataFusion SELECT][df-select], [logical plans][df-plan], [expressions][df-expr],
[aggregates][df-agg], [windows][df-window], [function volatility][df-volatility];
Prometheus [query basics][prom-basics], [operators][prom-operators],
[functions][prom-functions], [AST][prom-ast] and [function signatures][prom-signatures].
The [DataFusion release][df-release] and [Prometheus release][prom-release] identify
the targets; the parser's [compatibility declaration][parser-version] describes the
older repository dependency. This documentation change upgrades neither dependency.

### 3.2 SQL semantic mapping

| SQL construct / example | Representation in §2 | Coverage and semantic condition |
|---|---|---|
| `FROM t`, `WHERE x > 1`, `SELECT x * 2` | `Scan`, `Filter`, `Project`; scalar `Compare`, `Arithmetic` | **Direct.** Resolve columns against the input; predicates must be Boolean and only TRUE passes. |
| `VALUES (1), (2)`; `SELECT 1` | `Values`; `Project` over one empty row | **Direct.** SQL still returns a relation, unlike a PromQL scalar root. |
| Literals, columns, `CASE`, `CAST`, `TRY_CAST`, `IN (...)`, `IS NULL`, Boolean logic | Corresponding `ScalarExpr` variants | **Direct** within supported types. Preserve coercion, NULL propagation and conditional evaluation. Type gaps are listed below. |
| Unary minus, `BETWEEN`, `IS TRUE`, null-safe equality | `Negative`; compositions of `Compare`, `Case`, `IsNull`, Boolean expressions | **Direct/Lowered.** Evaluate reused nontrivial operands once, using a projected column where needed; do not duplicate volatile calls. |
| Scalar functions and `NOW()` | Resolved `FunctionCall`; `CurrentTimestamp` | **Direct** for registered contracts. Preserve stable/volatile evaluation behavior; unresolved names are unsupported. |
| Joins, cross joins, semi/anti joins | `Join` and its predicate | **Direct.** Predicate scope includes both inputs. Preserve outer-join null extension; ordinary anti-join is not nullable `NOT IN`. |
| `GROUP BY`, `SUM(x)`, `HAVING` | `Aggregate`, `Reduction`, existing `AggIntent`; scalar predicate on aggregate outputs | **Direct** for represented intents. `COUNT(*)` counts rows; `COUNT(x)` requires counting non-null values, not reusing row count unchanged. |
| `SUM(price * quantity)`; expression grouping keys | Input `Project`, then `Aggregate` referencing its output columns | **Lowered.** Current conversion rejects some expression arguments; the proposed representation can retain them. |
| `COUNT(x)` alongside other measures | Project a 0/1 non-null indicator, sum it, return 0 for an empty global group | **Lowered.** Filtering the whole input would incorrectly change the other measures. |
| `QUALIFY`, `DISTINCT ON` | `Filter` after window evaluation; ordered partitioned `Limit(n = Some(1))` | **Lowered.** Resolve aliases first; retain window/filter order and the selected row. |
| `ROLLUP`, `CUBE`, `GROUPING SETS` | One `Aggregate` per grouping set, projected missing keys and grouping discriminator, then `Concat` | **Lowered.** Retain duplicate grouping sets and distinguish omitted keys from input NULLs. |
| Wildcards, `UNION BY NAME`, pipe syntax for supported operations | Resolve columns, align with `Project`, compose the corresponding operators | **Lowered.** These syntax forms need no additional computational category. |
| `ROW_NUMBER()`, `LAG(x)`, `SUM(x) OVER (...)` | `SQLWindowFunc`, scalar args and sort keys; `Project` for expression partition keys | **Direct/Lowered** for existing `WindowFuncKind` and ROWS/RANGE frames. Window output is a column, not a scalar aggregate call. Missing features are listed below. |
| `DISTINCT`, `UNION`, `INTERSECT`, `EXCEPT`, their supported `ALL` forms | `Dedup`, `Concat` / `SetOp` | **Direct.** Preserve bag multiplicity and SQL duplicate/NULL equality rules. |
| `ORDER BY`, `LIMIT`, offset-only queries | `Sort`, `Limit` | **Direct.** Preserve direction and NULL placement; `n = None` means no fetch limit. |
| Uncorrelated scalar subquery, `EXISTS`, `IN` / `NOT IN (SELECT ...)` | Explicit scalar plan-reading variants | **Direct.** Preserve the cardinality and NULL contracts in §2.2, including when used in a SELECT list. |
| Derived tables and nonrecursive CTEs | Existing operator subgraphs; aliases resolved to output columns | **Lowered.** Naming alone needs no computation node. Reuse must not alter volatile evaluation. |

### 3.3 PromQL semantic mapping

Operator results must distinguish relations, instant vectors and range vectors;
`ScalarExpr` has a value type. Infer and validate these kinds from the source and
operation, rather than treating identical column schemas as interchangeable.
A range-query request evaluates its root at successive timestamps; it is not a
range-vector expression. The request permits scalar or instant-vector roots.

| PromQL construct / example | Representation in §2 | Coverage and semantic condition |
|---|---|---|
| Numeric/string literals; parentheses; `time()` | Scalar `Literal`, nested expression, `EvalTimestamp` | **Direct.** No bridge node; free columns are invalid at a scalar root. |
| Scalar arithmetic, unary minus, `1 < bool 2` | `Arithmetic`, `Negative`, `Case(Compare(...), 1.0, 0.0)` | **Direct/Lowered** with PromQL numeric semantics. Scalar comparison without `bool` is invalid. |
| `up{job="api"}` | Time-series `Scan` with predicates, `TimeRange(Instant)` | **Direct.** Label matching treats absent labels as empty and regexes as anchored. Selection uses lookback and staleness, not SQL row filtering alone. |
| `up[5m]` | `TimeRange(Range)` over a time-series scan | **Direct.** Retain samples and their timestamps in the left-open, right-closed window. |
| `-up` | `Project` with `Negative(sample)` | **Lowered** for float samples; retain series identity and unary-operation naming rules. |
| `up * 2`, `2 / up`, `up * scalar(sum(other))` | `Project` with scalar arithmetic and, where needed, `PromqlScalarFromVector` | **Lowered** for float samples. Preserve operand order, time/label fields and metric-name removal. Scalar input is evaluated in the same query-time context. |
| `up > 0`; `up > bool 0` | `Filter`; or `Project` with `Case(Compare(...), 1.0, 0.0)` | **Lowered** for float samples. Filtering preserves surviving sample values; bool mode produces numbers and removes the metric name. Scalar-on-left comparisons still retain the vector's sample value when filtering. |
| `a / on(job) group_left b`; `a > bool b` | `BinaryOp` with `vector_match`, `return_bool` | **Direct.** Retain matching cardinality, labels and metric-name rules; unmatched elements disappear, not become false rows. |
| `a and b`, `a or b`, `a unless b` | `BinaryOpKind::Set` | **Direct.** Label-set matching, not SQL Boolean evaluation or SQL bag set operations. |
| `sum by(job)(up)`, `avg without(instance)(up)` | `Aggregate(Reduction::Reduce(...), AggIntent)` | **Direct** for represented intents; preserve PromQL label grouping and empty-input behavior. |
| `topk(3, up)`, `bottomk(3, up)` with optional grouping | `Sort` + partitioned `Limit` | **Lowered** for constant parameters and float samples, retaining selected series labels and specified NaN ordering. Existing heavy-hitter `AggIntent::TopK` is not a substitute for sample-value ranking. |
| `rate(x[5m])`, `sum_over_time(x[5m])` | Range input + `Aggregate(Reduction::PerEntity, corresponding AggIntent)` | **Direct** for existing contracts: counter resets, extrapolation and range reduction belong to the named intent, not ordinary SQL SUM. |
| `vector(s)`, `scalar(v)` | `PromqlVectorFromScalar`; scalar `PromqlScalarFromVector` | **Direct.** These change result kind/cardinality and cannot be removed as representation wrappers. |
| `expr[30m:1m] offset 5m`, selectors with `@` | `PromqlSubquery`; surrounding `TimeShift` | **Direct.** Apply the anchor/offset to the whole child evaluation; preserve nested grids, default resolution and query-level start/end anchors. |
| Per-sample math/date functions, including scalar parameters | `Project` with a resolved scalar `FunctionCall` | **Lowered** for registered float-sample contracts. Scalar parameters can be expressions; do not require constant-only `AggIntent::Math` payloads. |
| Request-context functions and duration expressions, e.g. `x[max_of(step(), 5s)]` | Context-reading `FunctionCall`; resolve duration arithmetic before constructing `TimeRange` / `TimeShift` | **Lowered** when request parameters and the relevant subquery context determine the duration. A stored duration is valid only for that binding. |
| Relabeling, absence, `info`, series sampling | `PromqlRelabel`, relevant `AggIntent`, `PromqlInfoEnrich`, `PromqlSeriesSample` | **Partial.** Registered contracts must preserve label construction, missing-series behavior and feature gates. Parameter/type gaps below remain. |

### 3.4 Remaining semantic gaps

These are limits of the proposed payloads or an as-yet unspecified lowering, not
reasons to mix all operators and expressions back into one enum. The tables above
cover the core structures; they do not assert full language conformance.

| Semantics not fully covered | Why §2 cannot currently express it faithfully | Required extension or decision |
|---|---|---|
| General SQL aggregate `FILTER`, `DISTINCT`, internal ordering, null treatment and arbitrary aggregate UDFs | `AggIntent` is a fixed intent vocabulary with no general per-measure modifier payload. `COUNT(DISTINCT ...)` has `Cardinality`, but this does not cover every aggregate. | Define per-measure semantics or an equivalent lowering for each case. A filter on the whole aggregate input is not a general replacement. |
| All DataFusion window functions, GROUPS frames, explicit null treatment, window `FILTER` / `DISTINCT` | `WindowFuncKind` is a subset; `WindowFrameUnits` only has Rows/Range; the window payload lacks these modifier fields. | Extend these existing payloads for the requested features; ordinary scalar `FunctionCall` cannot supply window context. |
| Correlated SQL subqueries and recursive CTEs | Scalar plan references have no outer-scope binding, and an ordinary DAG has no recursive/fixpoint contract. | Define correlation scopes and recursive evaluation separately; only proven equivalent decorrelation is usable today. |
| SQL higher-order functions and lambda expressions | `FunctionCall.args` has no lambda parameter bindings or body scope; ordinary column references cannot stand for lambda variables. | Add explicit scalar lambda/binding structures before claiming coverage. |
| General SQL `ANY` / `ALL` subquery comparisons | `InSubquery` only records membership, not a comparison operator and quantifier. | Add a scalar `SetComparison` payload or a proven lowering retaining empty-set and NULL behavior. |
| Unbound SQL parameters / scalar variables | No placeholder or variable binding is represented. | Bind them to typed values before this IR, or define a binding payload; a free column is not a parameter. |
| Full DataFusion value/type fidelity | Existing `DataType`/`ScalarValue` cannot retain every decimal, unsigned width, timestamp unit/timezone or typed nested literal. Widening values can change result types and errors. | Extend the value/type model; casts cannot recover information already lost. The SQL mapping is restricted to faithfully represented types. |
| SQL pattern matching with explicit escape rules and all dialect-specific scalar operators | Current `CompareOpKind` pattern variants do not carry `ESCAPE`; function names alone do not define missing semantics. | Add the missing payload or a registered equivalent scalar contract. Simple LIKE does not prove ESCAPE support. |
| SQL `UNNEST` / general table functions | No operator expands a collection or invokes a table-valued function with its output cardinality/schema contract. | Add a relational operation, not a scalar function pretending to produce rows. |
| PromQL native-histogram samples and mixed-sample behavior | Existing value types have no native-histogram sample representation or annotation contract. Named histogram intents alone do not preserve those samples. | Extend sample types and define invalid-operation/annotation behavior; the float mappings above do not cover this case. |
| Dynamic PromQL parameters, e.g. `quantile(scalar(q), up)` | `AggIntent.q`, sampling parameters and `Limit.n` store constants rather than expression dependencies. Per-sample math can instead use `FunctionCall` as mapped above. | Permit scalar expressions in the relevant parameter positions. Constant folding only covers genuinely constant inputs. |
| PromQL experimental fill modifiers | `VectorMatch` has no left/right fill values, so it cannot distinguish dropping an unmatched series from supplying a numeric default. | Extend that payload for `fill`, `fill_left`, `fill_right`; preserve the `promql-binop-fill-modifiers` feature gate. |
| Extended PromQL range selectors and start-timestamp functions | `TimeRangeKind` has no anchored/smoothed selection mode, and the sample schema has no distinct start-timestamp metadata contract. | Define the temporal/sample metadata and feature gates before mapping these forms. A normal sample timestamp is not a start timestamp. |

### 3.5 Context and evaluation invariants

Scalar columns resolve against the owning input, both inputs for a join predicate,
and aggregate outputs for `HAVING`. Scan predicates use the source schema. SQL
subqueries in this proposal have their own scope and no implicit outer references.
Predicate wrapping does not bypass Boolean typing or SQL three-valued logic.

Function resolution preserves volatility: SQL `NOW()` is stable within a statement;
repeated volatile calls need not agree. Expression ownership does not authorize
copying, sharing or moving evaluations. PromQL scalar plan reads and
`EvalTimestamp` use the active evaluation instant, including within subqueries.
A reused operator must not be evaluated once and then incorrectly reused across
different time contexts.

## 4. Acceptance and scope

This is a representation proposal, not a runtime implementation or a declaration
of complete SQL/PromQL support. Acceptance requires:

- The SQL example in §1 retains its values, schema and predicate context.
- Every Direct/Lowered mapping in §3 has a valid typed representation; each Gap is
  explicitly rejected until its payload or equivalent lowering is defined.
- The scalar roots and mixed scalar/vector examples in §2.3 need no
  `PromqlScalarBridge` or equivalent constant-wrapper node.
- Operator dependencies inside scalar conversions/subqueries remain visible and
  shared; scalar trees remain owned. Invalid result-kind combinations are rejected.
- SQL NULL/cardinality rules, PromQL labels and evaluation times survive conversion.

Implementation will require frontend, validation and plan-format migration for
these explicit structural changes. DDL/DML, session commands, physical execution,
new optimization algorithms, accuracy and execution-timing policy are outside this
proposal. The companion document defines the common pre-/post-ASAP operator graph.

[df-release]: https://github.com/apache/datafusion/releases/tag/55.1.0
[prom-release]: https://github.com/prometheus/prometheus/releases/tag/v3.15.0
[df-select]: https://github.com/apache/datafusion/blob/55.1.0/docs/source/user-guide/sql/select.md
[df-plan]: https://github.com/apache/datafusion/blob/55.1.0/datafusion/expr/src/logical_plan/plan.rs
[df-expr]: https://github.com/apache/datafusion/blob/55.1.0/datafusion/expr/src/expr.rs
[df-agg]: https://github.com/apache/datafusion/blob/55.1.0/docs/source/user-guide/sql/aggregate_functions.md
[df-window]: https://github.com/apache/datafusion/blob/55.1.0/docs/source/user-guide/sql/window_functions.md
[df-volatility]: https://github.com/apache/datafusion/blob/55.1.0/datafusion/expr-common/src/signature.rs
[prom-basics]: https://github.com/prometheus/prometheus/blob/v3.15.0/docs/querying/basics.md
[prom-operators]: https://github.com/prometheus/prometheus/blob/v3.15.0/docs/querying/operators.md
[prom-functions]: https://github.com/prometheus/prometheus/blob/v3.15.0/docs/querying/functions.md
[prom-ast]: https://github.com/prometheus/prometheus/blob/v3.15.0/promql/parser/ast.go
[prom-signatures]: https://github.com/prometheus/prometheus/blob/v3.15.0/promql/parser/functions.go
[parser-version]: https://github.com/ProjectASAP/promql-parser/blob/9fede7eecca923c9882fe256484d00d37f8706cb/README.md#promql-compliance
