# Decoupling Operators From Scalar Expressions

> Status: proposal, not implemented. Audience: planner designers and architects.
> Companion to [Operator sharing](operator-sharing.md).

## 1. Problem and goal

The current query representation mixes table-producing operators and scalar
expressions. Their roles are distinguished by where they occur, so an expression
can be placed where a table input is expected and fail only when the plan is checked.

Separate these concepts so the plan model expresses which combinations are valid.
This also makes clear what the planner can replace or share as a computation.

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

## 2. Design and rationale

| Concept | Meaning | Role in the plan |
|---|---|---|
| Operator | Produces a table, or summary state in the companion design | A graph node whose input computations may be replaced or shared |
| Scalar expression | Computes a value in a particular schema context | Part of an operator's predicate, projection, sort key or other expression |

Operator inputs must be other operators. Scalar expressions may contain other
scalar expressions, but do not contain operator subplans in this design.

A column reference has meaning only in its schema context. Two identical-looking
expressions in different operators may refer to different inputs. Keeping scalar
expressions attached to their operators preserves that context; this proposal does
not introduce independent shared scalar computations.

The distinction depends on semantics, not on whether a result looks scalar. For
example, a PromQL conversion between a scalar and a vector participates in the
operator graph because it has query-level output semantics. SQL `NOW()` is an
expression evaluated within its owning operator.

## 3. Semantic requirements

Column resolution keeps its existing meaning:

- Most expressions use the input operator's output schema.
- A join predicate uses both input schemas.
- An aggregate's `HAVING` expression uses the aggregate output schema.
- A scan predicate uses the scanned data's schema.

Splitting the representation must preserve evaluation behavior, inferred output
types and source-language semantics. A scalar expression cannot serve as a table
input, and a table-producing operator cannot appear where a scalar is expected.

The [operator-sharing proposal](operator-sharing.md#11-unified-operator-type)
extends the operator graph with summary operations. The scalar/operator distinction
continues to hold before and after that optimization: replacing a projection's input
with a summary estimate does not turn its scalar expressions into graph nodes.

## 4. Acceptance and scope

The example query must retain its result and output schema. Planning can replace or
share its table-producing computations while interpreting the filter predicate and
projection expression in the correct contexts. Invalid scalar/table combinations
must be excluded by the plan model.

This separation alone does not require a change to the external plan format.
The companion proposal addresses the separate decision to expose every operator in
the exported graph.

Scalar subqueries are outside this design. Filter `IN (SELECT …)` and `EXISTS` can
be represented as joins, but a general scalar subquery introduces a dependency on
another operator graph. Supporting that requires a separate design for its scope,
dependencies and participation in optimization.
