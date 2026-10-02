# Pre-ASAP IR

Pre-ASAP IR is ASAPPlanner's exact, language-independent representation of query intent. It deliberately contains no summary or sketch choice. Equivalent SQL, PromQL, and future-language queries should produce the same intent shape when they mean the same thing.

Only semantics that affect correctness, summary applicability, or cost become first-class nodes. For field definitions, invariants, and examples, use the [Pre-ASAP IR reference](../../develop_docs/pre-asap-ir.md).

## Node catalog

### Aggregation

- Aggregate — reduces input entities or computes a per-entity aggregate intent.

### Time

- TimeRange — PromQL sample selection: an instant selector's lookback, or a range selector such as [5m].
- TimeShift — moves when a selector is evaluated (offset or @).
- PromqlSubquery — re-evaluates an instant-vector expression over a range.

### Relational

- Scan — identifies a logical data source.
- Values — literal rows; one empty row is the input of a `SELECT` without `FROM`.
- ScalarBridge — a scalar expression at an operator position: a bare scalar query, or the scalar operand of `<vector> op <scalar>`.
- Filter — restricts rows using a predicate.
- Project — selects or derives output columns.
- BinaryOp — composes two inputs with arithmetic, comparison, or boolean logic. A PromQL `bool` comparison returns 0/1 instead of filtering.
- Sort — orders rows without expressing a heavy-hitter intent.
- Limit — caps a row count, optionally after an offset.
- Dedup — removes duplicate rows.
- Join — combines two logical inputs.
- SetOp — represents typed SQL set operations.
- Concat — concatenates union-compatible branches without deduplication.

### PromQL-specific

- PromqlVectorFromScalar — promotes a scalar to a label-less instant vector. Its inverse, PromQL `scalar(v)`, is a scalar expression that reads `v`.
- PromqlRelabel — rewrites labels on each series.
- PromqlInfoEnrich — enriches labels from an info metric.
- PromqlSeriesSample — selects whole series without reducing them.

### SQL-specific

- SQLWindowFunc — evaluates an analytic function with an OVER (...) clause.
