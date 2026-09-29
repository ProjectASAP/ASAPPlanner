# Decoupling Operators From Scalar Expressions

> Status: proposed, not implemented. Companion to [Operator sharing](operator-sharing.md)
> (same PR): this document splits `QueryExpr`; that one builds the shared operator
> language on the result. Code is referenced by file and function; counts are
> approximate, measured on `main` at `8acb472`.

**The idea.** `QueryExpr` holds two different kinds of node in one enum. This proposal
splits it into `NonASAPOp` (operators) and `ScalarExpr` (scalar expressions), so the
field a node sits in decides its type.

```
Today                                        Proposed
Filter { pred:  Rc<QueryExpr>,               Filter { pred:  Predicate(Rc<ScalarExpr>),
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

- An **operator** outputs a table. It is a node of the plan: the planner can replace it,
  share it, or put a summary under it.
- A **scalar expression** has no table of its own. `Column(4)` means "column 4 of the
  input of the operator I sit in"; outside that operator it means nothing.

Today both are `QueryExpr` variants, told apart only by field position. The code already
separates them, but only by convention:

- `QueryExpr::output_schema` returns `ScalarHasNoRowSchema` for all 13 scalar variants
  (`query_expr.rs`), so `Filter { child: Literal(2) }` compiles and fails at run time.
- `pre_asap/cse.rs` never descends into a scalar, and repeats a "scalar: nothing to do"
  arm in each of its three traversals; `canonicalize` likewise never rewrites one.

## 2. Types

```rust
pub enum NonASAPOp<C: ColState = ColumnId> {
    Scan { .. }, Filter { pred: Predicate<C>, child: Rc<NonASAPOp<C>> }, Project { cols: Vec<ProjectItem<C>>, child },
    Aggregate { .. }, Join { .. }, SetOp { .. }, Concat { .. }, Dedup { .. }, Sort { .. }, Limit { .. }, BinaryOp { .. },
    SQLWindowFunc { .. }, TimeRange { .. }, TimeShift { .. }, Promql* { .. },
    ScalarBridge(Rc<ScalarExpr<C>>),   // formerly PromqlScalarBridge (the `2` in PromQL `v * 2`)
    EvalTimestamp,                     // PromQL time()
}
pub enum ScalarExpr<C: ColState = ColumnId> {
    Column(C), Literal(ScalarValue), Compare { .. }, BoolAnd(..), BoolOr(..), Not(..), IsNull(..), IsNotNull(..),
    Cast { .. }, InList { .. }, FunctionCall { .. }, Arithmetic { .. }, Case { .. }, CurrentTimestamp,
}
pub struct Predicate<C>(pub Rc<ScalarExpr<C>>);
pub struct ProjectItem<C> { pub alias: Option<String>, pub expr: ScalarExpr<C> }
```

```text
NonASAPOp<C>
├─ children:      Rc<NonASAPOp<C>>             (Rc<Operator<C>> after operator sharing)
└─ scalar fields: Predicate<C> / ProjectItem<C> / SortKey<C> / ScalarBridge / ...
                    └─ ScalarExpr<C>
                         └─ children: ScalarExpr<C> only, never an operator
```

- **Naming**: `NonASAPOp` is named for [Operator sharing](operator-sharing.md), where it
  becomes the non-ASAP category of `Operator`. `QueryExpr` goes away.
- **Scalar fields**: `Filter.pred`, `Join.pred`, `Aggregate.having` (`Predicate`);
  `Project.cols` (`ProjectItem`); `Sort` / `SQLWindowFunc` sort keys (`SortKey`);
  `SQLWindowFunc.args`; `PromqlRelabel.value`.
- **Borderline variants** go by position, not by look. `PromqlScalarBridge`,
  `EvalTimestamp`, `PromqlScalarFromVector` and `PromqlVectorFromScalar` sit in operator
  position with a row schema (e.g. a `BinaryOp` operand) → `NonASAPOp`. `CurrentTimestamp`
  (SQL `NOW()`) is produced only by scalar lowering (`df_expr_to_unresolved`); its only
  operator-position use is in a unit test → `ScalarExpr`.
- **Gain**: neither a scalar in operator position nor an operator in scalar position is
  expressible, and `ScalarHasNoRowSchema` is deleted.

## 3. Changes

| Location | Change |
|---|---|
| frontend expression lowering (`df_expr_to_unresolved`, PromQL `walk`) | scalar positions build `ScalarExpr`, operator positions `NonASAPOp` |
| `resolve`, `column_resolution.rs` | already separate: `resolve` walks operators and calls `resolve_expr` for scalars. Each scalar resolves against one schema its operator picks (usually the child's output; the `Aggregate`'s output for `HAVING`, left + right for a `Join` predicate, the `Scan`'s own schema for `Scan` predicates). The split only changes their signatures: `resolve` takes `NonASAPOp`, `resolve_expr` takes `ScalarExpr`. Leaf schemas are still inferred from scalar column references across the whole tree |
| `canonicalize`, `pre_asap/cse.rs` | the "scalar: nothing to do" arms go; scalars are hashed as plain data |
| `scalar_signature.rs`, `infer_expr_type` | take `ScalarExpr` |
| `QueryExpr::output_schema` | becomes `NonASAPOp::output_schema`; the scalar arms and `ScalarHasNoRowSchema` go |

## 4. Implementation and tests

This is stage 1 of the joint plan ([Operator sharing §8](operator-sharing.md#8-stages-and-tests)):
children stay `Rc<NonASAPOp>`; operator sharing widens them to `Rc<Operator>` in its
stage 2.

**No wire change.** The `fallback` payload serializes a `QueryExpr`, externally tagged.
Variant names are kept, so a tree serializes the same; `ScalarBridge` keeps the name
`PromqlScalarBridge` with `#[serde(rename)]`.

- Existing tests pass unchanged apart from construction syntax.
- Tests that place a scalar in operator position no longer compile and are rewritten or
  deleted: the `CurrentTimestamp` unit test, the `ScalarHasNoRowSchema` tests, and the
  `executable_dag.rs` tests using `QueryExpr::Literal` as a `fallback` expression.

## 5. Limits

The split relies on no scalar containing an operator. That holds today: SQL
`IN (SELECT …)` / `EXISTS` in a filter lower to a semi-join (`lower_filter`), and every
other subquery-valued expression is rejected (`frontend-sql/src/sql/expr.rs`). Supporting
a scalar subquery (`WHERE x > (SELECT avg(x) …)`) would add `ScalarExpr::Subquery(Rc<..>)`,
make the two types mutually recursive, and require CSE and the planner to look inside
scalars.
