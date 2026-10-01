# Decoupling Operators From Scalar Expressions

> Status: proposal, not implemented. Audience: planner designers and architects.
> Companion to [Operator sharing](operator-sharing.md).

## 1. Problem and goal

The current query representation mixes table-producing operators and scalar
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

An **operator** describes a query computation with its own output schema. It reads
from a source or other operators and produces a relation or time-series result.
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

The sketches retain existing `Rc` and collection shapes where possible; this
proposal does not redesign storage. `C` remains the existing column representation:
`ColumnRef` before resolution and `ColumnId` afterward. `C::ScanSchema` retains the
corresponding unresolved or resolved scan schema.

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
        children: Vec<NonASAPOp<C>>,
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
    PromqlScalarBridge(Rc<ScalarExpr<C>>),
    EvalTimestamp,
    PromqlVectorFromScalar(Rc<NonASAPOp<C>>),
    PromqlScalarFromVector(Rc<NonASAPOp<C>>),
    PromqlRelabel { dst: String, value: Rc<ScalarExpr<C>>, child: Rc<NonASAPOp<C>> },
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
struct Predicate<C: ColState = ColumnId>(Rc<ScalarExpr<C>>);

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

`Predicate`, `ProjectItem` and `SortKey` keep their current names and roles. Only the
expression type changes. A column reference remains meaningful in the schema
selected by its owning operator; it is not an independent table input.

This is the scalar/operator split alone. It retains the current pre-ASAP `BinaryOp`,
`Limit` and `Concat` shapes. The [operator-sharing proposal](operator-sharing.md#11-unified-operator-type)
separately widens operator inputs to the common `Operator` and reconciles differences
between pre-ASAP and post-ASAP operations.

### 2.2 Scalar expressions

`ScalarExpr` contains every current scalar variant, including `CurrentTimestamp`.
Its recursive inputs are scalar expressions only.

```rust
enum ScalarExpr<C: ColState = ColumnId> {
    Column(C),
    Literal(ScalarValue),
    Compare { left: Rc<ScalarExpr<C>>, op: CompareOpKind, right: Rc<ScalarExpr<C>> },
    BoolAnd(Vec<ScalarExpr<C>>),
    BoolOr(Vec<ScalarExpr<C>>),
    Not(Rc<ScalarExpr<C>>),
    IsNull(Rc<ScalarExpr<C>>),
    IsNotNull(Rc<ScalarExpr<C>>),
    Cast { expr: Rc<ScalarExpr<C>>, to: DataType, try_cast: bool },
    InList { expr: Rc<ScalarExpr<C>>, list: Vec<ScalarExpr<C>>, negated: bool },
    FunctionCall { name: String, args: Vec<ScalarExpr<C>> },
    Arithmetic {
        op: ArithmeticOpKind, left: Rc<ScalarExpr<C>>, right: Rc<ScalarExpr<C>>,
    },
    Case {
        operand: Option<Rc<ScalarExpr<C>>>,
        branches: Vec<(ScalarExpr<C>, ScalarExpr<C>)>,
        else_expr: Option<Rc<ScalarExpr<C>>>,
    },
    CurrentTimestamp,
}
```

`PromqlScalarBridge`, `PromqlVectorFromScalar`, `PromqlScalarFromVector` and
`EvalTimestamp` remain operators because they participate in query-level evaluation.
`CurrentTimestamp` (SQL `NOW()`) belongs to scalar expressions. Classifying these by
their role preserves their existing semantics.

## 3. Semantic requirements

Column resolution keeps its existing meaning:

- Most expressions use the input operator's output schema.
- A join predicate uses both input schemas.
- An aggregate's `HAVING` expression uses the aggregate output schema.
- A scan predicate uses the scanned data's schema.

Splitting the representation must preserve evaluation behavior, inferred output
types and source-language semantics. A scalar expression cannot serve as a table
input, and a table-producing operator cannot appear where a scalar is expected.

## 4. Acceptance and scope

The example query must retain its result and output schema. The filter predicate
and projection expression must resolve in the same contexts as before. Invalid
scalar/table combinations must be excluded by the plan model.

This separation alone does not require a change to the external plan format.
The companion proposal addresses the separate decision to expose every operator in
the exported graph.

Scalar subqueries are outside this design. Filter `IN (SELECT …)` and `EXISTS` can
be represented as joins; a general scalar subquery would require expressions to
reference operator graphs and needs a separate design. This proposal adds no new
optimization, accuracy or execution-timing behavior.
