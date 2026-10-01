# Decoupling Operators From Scalar Expressions

> Status: proposal, not implemented. Audience: planner developers and designers.
> Companion to [Operator sharing](operator-sharing.md). Code references use `main`
> at `5a32b8b`.

Split `QueryExpr` into `NonASAPOp` for table-producing operators and `ScalarExpr`
for expressions evaluated within an operator. This makes invalid combinations,
such as a literal used as a filter's table input, unrepresentable.

```
Today                                        Proposed
Filter { pred:  Rc<QueryExpr>,               Filter { pred:  Predicate(ScalarExpr),
         child: Rc<QueryExpr> }                       child: Rc<NonASAPOp> }
```

## 1. Problem

```
SELECT l_quantity * 2 AS q2 FROM lineitem WHERE l_quantity > 10

Project                              ← operator: outputs a table
  cols: [Column(4) * Literal(2)]     ← scalar expression: outputs one value per input row
  child: Filter                      ← operator
    pred: Column(4) > Literal(10)    ← scalar expression
    child: Scan lineitem             ← operator
```

Operators produce tables and can be replaced or shared by the planner. Scalars
compute values within the schema chosen by their operator; `Column(4)` has no meaning without
that context.

Today both are `QueryExpr` variants, separated only by convention:

- `Filter { child: Literal(2) }` compiles, then fails with `ScalarHasNoRowSchema`.
- CSE and canonicalization skip scalars using repeated special-case branches.

## 2. Types

```rust
pub enum NonASAPOp<C: ColState = ColumnId> {
    Scan { .. }, Filter { pred: Predicate<C>, child: Rc<NonASAPOp<C>> }, Project { cols: Vec<ProjectItem<C>>, child },
    Aggregate { .. }, Join { .. }, SetOp { .. }, Concat { .. }, Dedup { .. }, Sort { .. }, Limit { .. }, BinaryOp { .. },
    SQLWindowFunc { .. }, TimeRange { .. }, TimeShift { .. }, Promql* { .. },
    ScalarBridge(ScalarExpr<C>),       // formerly PromqlScalarBridge (the `2` in PromQL `v * 2`)
    EvalTimestamp,                     // PromQL time()
}
pub enum ScalarExpr<C: ColState = ColumnId> {
    Column(C), Literal(ScalarValue), Compare { .. }, BoolAnd(..), BoolOr(..), Not(..), IsNull(..), IsNotNull(..),
    Cast { .. }, InList { .. }, FunctionCall { .. }, Arithmetic { .. }, Case { .. }, CurrentTimestamp,
}
pub struct Predicate<C>(pub ScalarExpr<C>);
pub struct ProjectItem<C> { pub alias: Option<String>, pub expr: ScalarExpr<C> }
```

Operators own their scalar fields by value. CSE hashes these fields as data; scalars
are not shared DAG nodes. Recursive scalar fields still require indirection such as
`Box` or `Vec`; the type sketches omit those details.

```text
NonASAPOp<C>
├─ children:      Rc<NonASAPOp<C>>             (Rc<Operator<C>> after operator sharing)
└─ scalar fields: Predicate<C> / ProjectItem<C> / SortKey<C> / ScalarBridge / ...
                    └─ ScalarExpr<C>
                         └─ children: ScalarExpr<C> only, never an operator
```

`QueryExpr` is removed. `NonASAPOp` is named for its role in the companion proposal.
Scalar fields include predicates (`Filter`, `Join`, `HAVING`), project items, sort
keys, window-function arguments and relabel values.

Classify ambiguous variants by their position and schema:

- `ScalarBridge`, `EvalTimestamp`, `PromqlScalarFromVector` and
  `PromqlVectorFromScalar` remain operators because they have row schemas.
- `CurrentTimestamp` (SQL `NOW()`) is a scalar. Its only operator-position use today
  is a unit test.

The split prevents scalars in operator positions and operators in scalar positions,
eliminating `ScalarHasNoRowSchema`.

## 3. Changes

| Location | Change |
|---|---|
| frontend expression lowering (`df_expr_to_unresolved`, PromQL `walk`) | scalar positions build `ScalarExpr`, operator positions `NonASAPOp` |
| `resolve`, `column_resolution.rs` | `resolve` takes `NonASAPOp`; `resolve_expr` takes `ScalarExpr`. Schema selection and leaf-schema inference are unchanged. |
| `canonicalize`, `pre_asap/cse.rs` | the "scalar: nothing to do" arms go; scalars are hashed as plain data |
| `scalar_signature.rs`, `infer_expr_type` | take `ScalarExpr` |
| `QueryExpr::output_schema` | becomes `NonASAPOp::output_schema`; the scalar arms and `ScalarHasNoRowSchema` go |

Scalars resolve against the schema chosen by their operator: usually the child's
output, the aggregate output for `HAVING`, both inputs for a join predicate, or the
scan schema for a scan predicate. Leaf schemas still use column references across
the whole tree.

## 4. Implementation and tests

This is [stage 1 of operator sharing](operator-sharing.md#8-stages-and-tests).
Children stay `Rc<NonASAPOp>` until stage 2 widens them to `Rc<Operator>`.

The wire format stays unchanged: preserve the externally tagged variant names,
including `PromqlScalarBridge` via `#[serde(rename)]`.

Update construction syntax in existing tests. Rewrite or remove tests that put
scalars in operator positions: the `CurrentTimestamp`, `ScalarHasNoRowSchema`, and
literal-as-`fallback` cases.

## 5. Limits

Scalars currently contain no operators: filter `IN (SELECT …)` and `EXISTS` lower
to semi-joins; other subquery-valued expressions are rejected
(`frontend-sql/src/sql/expr.rs`). Supporting scalar subqueries later would require
mutually recursive types and traversal into scalars by CSE and the planner.
