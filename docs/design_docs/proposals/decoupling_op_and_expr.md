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

An **operator** describes a query-plan computation and its input dependencies.
Its result may be a relation, a time-series vector or a query-level scalar; the
plan must retain the result kind as well as its value types or output schema.
For example, `Scan` produces input rows, `Filter` selects rows, and `Project`
produces a new set of columns. Operators form the nodes and input dependencies of
the query plan.

A **scalar expression** describes a value calculated within the context of an
operator. For example, `l_quantity > 10` produces a Boolean used by a filter, and
`l_quantity * 2` produces a value for a projected column. An expression has a value
type, but no table output schema or independent data source. Its column references
are interpreted using the schema chosen by the owning operator.

In the example above, `Filter.child` is the `Scan` operator that supplies rows;
`Filter.pred` is the expression `l_quantity > 10` evaluated on those rows. The
predicate cannot replace the scan as the filter's input, and the scan cannot be
used as its Boolean condition. This is the distinction the two types enforce.

Split `QueryExpr` into `NonASAPOp` and `ScalarExpr`. Keep existing variant names,
field names and semantics; change their types to express their roles:

| Position | Current type | Proposed type |
|---|---|---|
| Operator input | `QueryExpr` | `NonASAPOp` |
| Scalar expression | `QueryExpr` | `ScalarExpr` |

The split also makes ownership consistent with each structure's role:

| Structure | Proposed rule | Reason |
|---|---|---|
| Operator inputs | Shared references, including every `Concat` child | All inputs are graph nodes; a multi-input operator should not embed copies while other operators reference nodes. |
| Scalar fields on operators | Owned `ScalarExpr` values, including predicates, relabel values and scalar bridges | These expressions belong to the operator's schema context and need no independent graph identity. |
| Scalar recursion | `Box<ScalarExpr>` for individual recursive children; `Vec<ScalarExpr>` for lists | Scalars form owned expression trees. `Box` gives recursive single-child fields finite size; `Vec` already provides that indirection for lists. |
| Column parameter | Keep `C: ColState` on operators; use plain `C` on scalar expressions and their wrappers | Operators need `C::ScanSchema`; scalar expressions only carry column references. |

`C` continues to represent `ColumnRef` before resolution and `ColumnId` afterward.
No new column trait is introduced. Required cloning, comparison or serialization
bounds belong on the corresponding implementations, not on scalar data structures
through the unrelated scan-schema requirement.

These are deliberate changes from the current mixed `Rc`/value representation.
They preserve operation names and evaluation semantics; they do not add scalar
sharing or a new optimization policy.

### 2.1 Operator nodes

`NonASAPOp` contains the current operator variants. Inputs reference other operators;
predicates and expression-bearing fields use the scalar structures below.

```rust
enum NonASAPOp<C: ColState = ColumnId> {
    Scan {
        source: Source, predicates: Vec<Predicate<C>>, schema: C::ScanSchema,
    },
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
    Limit { n: usize, offset: usize, child: Rc<NonASAPOp<C>> },
    BinaryOp {
        op: BinaryOpKind, lhs: Rc<NonASAPOp<C>>, rhs: Rc<NonASAPOp<C>>,
        vector_match: Option<VectorMatch>,
    },
    SQLWindowFunc {
        func: WindowFuncKind, args: Vec<ScalarExpr<C>>, partition_by: GroupKeys<C>,
        order_by: Vec<SortKey<C>>, frame: Option<WindowFrame>,
        output_name: String, child: Rc<NonASAPOp<C>>,
    },
    TimeRange { range: Duration, child: Rc<NonASAPOp<C>> },
    TimeShift { shift: TimeShift, child: Rc<NonASAPOp<C>> },
    PromqlScalarBridge(ScalarExpr<C>),
    EvalTimestamp,
    PromqlVectorFromScalar(Rc<NonASAPOp<C>>),
    PromqlScalarFromVector(Rc<NonASAPOp<C>>),
    PromqlRelabel { dst: String, value: ScalarExpr<C>, child: Rc<NonASAPOp<C>> },
    PromqlInfoEnrich { selector: Vec<InfoMatcher>, child: Rc<NonASAPOp<C>> },
    PromqlSeriesSample { by: GroupKeys<C>, kind: SampleKind, child: Rc<NonASAPOp<C>> },
    PromqlSubquery {
        range: Duration, resolution: Option<Duration>, child: Rc<NonASAPOp<C>>,
    },
}
```

The following structures describe scalar-bearing fields of `NonASAPOp`:
`Predicate` is used by filters, joins and `HAVING`; `ProjectItem` by projections;
and `SortKey` by sorting and window functions. Their expressions use `ScalarExpr`,
defined in §2.2.

```rust
struct Predicate<C = ColumnId>(ScalarExpr<C>);

struct ProjectItem<C = ColumnId> {
    alias: Option<String>,
    expr: ScalarExpr<C>,
}

struct SortKey<C = ColumnId> {
    expr: ScalarExpr<C>,
    ascending: bool,
    nulls_first: bool,
}
```

`Predicate`, `ProjectItem` and `SortKey` retain their names and roles and consistently
own their scalar expressions. `Predicate` marks a Boolean-expression position;
`ProjectItem` adds an alias, and `SortKey` adds ordering and null-placement rules.
These are different operator requirements, so the wrappers remain separate.

The pre-ASAP `BinaryOp` and `Limit` payloads remain unchanged. `Concat` now uses the
same shared-input representation as other operators. The
[operator-sharing proposal](operator-sharing.md#11-unified-operator-type) separately
widens those inputs to the common `Operator` and reconciles pre-ASAP/post-ASAP
operation differences.

### 2.2 Scalar expressions

`ScalarExpr` contains every current scalar variant, including `CurrentTimestamp`.
Its recursive inputs are scalar expressions only. Individual recursive children
are boxed; variable-length children are owned lists. Neither form gives a scalar
expression shared DAG-node identity.

```rust
enum ScalarExpr<C = ColumnId> {
    Column(C),
    Literal(ScalarValue),
    Compare { left: Box<ScalarExpr<C>>, op: CompareOpKind, right: Box<ScalarExpr<C>> },
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
    },
    Case {
        operand: Option<Box<ScalarExpr<C>>>,
        branches: Vec<(ScalarExpr<C>, ScalarExpr<C>)>,
        else_expr: Option<Box<ScalarExpr<C>>>,
    },
    CurrentTimestamp,
}
```

`PromqlScalarBridge`, `PromqlVectorFromScalar`, `PromqlScalarFromVector` and
`EvalTimestamp` remain operators because they participate in query-level evaluation.
`CurrentTimestamp` (SQL `NOW()`) belongs to scalar expressions. Classifying these by
their role preserves their existing semantics.

## 3. Semantic requirements

The split separates roles in the plan, not source-language result types. These
requirements define valid plans; retaining an existing variant does not establish
that its current implementation meets every requirement below.

### 3.1 Expression context and result types

Column resolution keeps its existing meaning:

- Most expressions use the input operator's output schema.
- A join predicate uses both input schemas.
- An aggregate's `HAVING` expression uses the aggregate output schema.
- A scan predicate uses the scanned data's schema.

A resolved `Predicate` must have Boolean type under the expression-typing and
coercion rules. SQL conditions retain three-valued logic: `FALSE` and `NULL` do not
pass a filter. Wrapping a numeric value in `Predicate` does not make it Boolean.

PromQL scalar, instant-vector and range-vector are query result kinds, distinct
from the internal `ScalarExpr` role. For example, `EvalTimestamp` and
`PromqlScalarFromVector` remain operators even though they produce scalar results.
Their consumers must validate the required result kind; a column schema alone
must not make a scalar interchangeable with a vector.

`PromqlScalarBridge` has no input schema and retains its current restricted role:
lifting a constant-folded numeric literal into the plan. It cannot accept free
column references. Other scalar-valued queries use operator nodes, including the
existing scalar/vector conversions; PromQL scalar does not mean constant.

### 3.2 PromQL binary and temporal operations

`BinaryOp` operates on query results, including vector matching and label rules.
`ScalarExpr::Arithmetic` and `ScalarExpr::Compare` compute values within an
operator's context. Likewise, PromQL set operations remain distinct from SQL
`SetOp` and scalar Boolean expressions.

PromQL comparison must distinguish filtering from the `bool` mode: `up > 0`
filters samples, whereas `up > bool 0` produces numeric `0` or `1` for matched,
valid samples. Scalar/scalar comparisons require `bool`. The current `BinaryOp`
payload and frontend conversion do not retain this mode. Supporting it requires
preserving the distinction; otherwise the frontend must reject it rather than
silently change the result.

`TimeRange`, `PromqlSubquery` and `SQLWindowFunc` retain separate roles. A PromQL
subquery evaluates its input over a time grid and produces a range vector; a SQL
window computes values over partitions and frames of input rows. Subquery range,
resolution, `offset` and `@` must retain their meaning and scope. The current
subquery conversion retains only range, resolution and child. A design using the
existing `TimeShift` must specify how it shifts or fixes the subquery evaluation
time, including nested subqueries; unsupported modifiers must be rejected.

These distinctions follow the PromQL references for
[result types and subqueries](https://prometheus.io/docs/prometheus/latest/querying/basics/)
and [binary operations](https://prometheus.io/docs/prometheus/latest/querying/operators/).

### 3.3 SQL aggregate, window and subquery boundaries

DataFusion 43's [`Expr`](https://github.com/apache/datafusion/blob/43.0.0/datafusion/expr/src/expr.rs)
includes aggregate, window and subquery expressions as well as ordinary scalar
expressions. It therefore does not map directly to this proposal's `ScalarExpr`.
Aggregate and window computations belong to `Aggregate` and `SQLWindowFunc`;
subsequent scalar expressions reference their output columns.

Where an operator accepts only column references, an expression argument must be
computed by an input `Project` or explicitly rejected. For example,
`SUM(price * quantity)` can become a projection of the product followed by an
aggregate over that column. The current aggregate conversion rejects such
arguments; this example describes a valid extension, not existing support.
The same rule applies to expression-valued grouping and partition keys.

Aggregate `FILTER`, `DISTINCT`, internal `ORDER BY`, and aggregate/window null
treatment must be preserved, translated equivalently or explicitly rejected.
They cannot be dropped merely because the current payload lacks a field.
General scalar subqueries remain outside this proposal's supported scope, as
specified in §4; the split does not claim full DataFusion SQL coverage.

### 3.4 Evaluation behavior

Owning or copying a `ScalarExpr` does not authorize changing how often it is
evaluated. Function resolution must retain the typing and evaluation properties
needed to preserve semantics, whether through the existing function catalog or
another established resolution mechanism.

DataFusion distinguishes
[immutable, stable and volatile functions](https://github.com/apache/datafusion/blob/43.0.0/datafusion/expr-common/src/signature.rs).
`CurrentTimestamp` (`NOW()`) stays stable within a SQL query; separate calls to a
volatile function such as `random()` may differ. Expression copying, sharing or
movement must respect those properties. `EvalTimestamp` instead follows the
PromQL evaluation timestamp, including evaluation inside a subquery.

## 4. Acceptance and scope

The example query must retain its result and output schema. The filter predicate
and projection expression must resolve in the same contexts as before. Invalid
scalar/table combinations must be excluded by the plan model. In particular:

- A `Concat` input has the same graph-node identity behavior as any other input.
- A scalar expression belongs to its owning operator; changing one operator's
  expression cannot implicitly change another operator's expression.
- Non-Boolean resolved predicates and column-dependent scalar bridges are rejected.
- `CurrentTimestamp` remains scalar, and `EvalTimestamp` remains an operator with
  its existing PromQL evaluation-time semantics.

This separation alone does not require a change to the external plan format.
The companion proposal addresses the separate decision to expose every operator in
the exported graph.

Scalar subqueries are outside this design. Filter `IN (SELECT …)` and `EXISTS` can
be represented as joins; a general scalar subquery would require expressions to
reference operator graphs and needs a separate design. This proposal adds no new
optimization, accuracy or execution-timing behavior.
