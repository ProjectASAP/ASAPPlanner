# ASAPController — Outstanding Work

Tracks what is built, what is stubbed, and what has not been started.
Organized from most immediate (unblocks the next step) to longest-horizon.

---

## 1. Immediate code stubs (`todo!()` / unit-struct stubs)

These are compile-time safe but will panic or silently drop information at runtime.

### `crates/core` — expression stubs

| Status | Location | What | Impact |
|---|---|---|---|
| ✅ done | `intent_algebra/expr.rs` | `AggIntent::requires() -> DataModel` | L4 rules can now gate on data model |
| ✅ done | `intent_algebra/expr.rs` | `AggIntent::output_type()` | L3 schema derivation for `Aggregate` works |
| ✅ done | `intent_algebra/expr.rs` | `HasSchema::output_schema()` — Scan, pass-through, Aggregate | Typed edges exist for all SQL-path nodes |
| ✅ done | `intent_algebra/expr_ir.rs` | `L3Expr` IR — `L3Scalar`, `CompareOp`, `L3Expr` with `conjuncts` / `columns_referenced` | Predicate, ProjectItem, SortKey now carry real expression content |
| ✅ done | `intent_algebra/expr.rs` | `Predicate(L3Expr)` — wraps a real filter expression | Filter nodes carry inspectable predicates |
| ✅ done | `intent_algebra/expr.rs` | `ProjectItem { expr: L3Expr, alias }` | Project nodes carry column expressions with aliases |
| ✅ done | `intent_algebra/expr.rs` | `SortKey { expr: L3Expr, ascending, nulls_first }` | Sort keys carry direction and expression |
| ✅ done | `lower/sql.rs` | `df_expr_to_l3` — DataFusion `Expr` → `L3Expr` translator | Lowerer populates predicates, project items, sort keys |
| ✅ done | `intent_algebra/expr_ir.rs` | `ArithOp` enum + `L3Expr::Arith` — binary arithmetic in expression IR | Arithmetic in predicates and projections now fully lowerable |
| ✅ done | `intent_algebra/expr_ir.rs` | `L3Expr::Case` — CASE WHEN / CASE expr WHEN | SQL CASE lowers to inspectable IR node |
| ✅ done | `intent_algebra/expr_ir.rs` | `CompareOp::ILike / NotILike` — case-insensitive LIKE | ILIKE predicates now in the IR |
| ✅ done | `intent_algebra/expr.rs` | `Project` schema for non-column items: `Cast → to`, `Literal → scalar type`, `Arith/Case/FunctionCall → Float64` default | `todo!()` removed; `populate_schemas` no longer panics on computed projections |
| ⬜ blocked | `intent_algebra/expr.rs` | `WindowFrame`, `VectorMatch`, `LabelFilter`, `MetricRef`, `PartitionKeys`, `JoinKey` — unit structs | PromQL and join paths structurally incomplete; unblocked only when those paths are implemented |

---

## 2. `crates/lower` gaps

| Status | Item |
|---|---|
| ✅ done | `lower_batch` integration tests — empty batch, per-query success, per-query error isolation |
| ✅ done | Language guard: `lower_batch` checks `workload.language`; returns `Err(LoweringError::WrongLanguage)` for non-SQL dialects. |
| ✅ done | `Source::Table.columns` — populated from the enclosing `Projection` node's column refs (DataFusion unoptimized plan never sets `TableScan.projection`; `SELECT *` leaves columns empty = "all columns"). |
| ✅ done | Time-range extraction: `BETWEEN low AND high` on the time column now folds both bounds into `Source::Table.time_range` (previously returned as non-time residual). Recursive lowering already handles `Filter → Aggregate → Filter → TableScan` correctly; the only real gap was `Expr::Between`. |
| ✅ done | Multi-dialect SQL guard — `SQL(ClickhouseSQL \| ElasticSQL)` now returns `LoweringError::UnsupportedDialect` instead of silently falling through to DataFusion's parser. Only `SQL(DataFusionSQL)` and `QueryLanguage::DataFusion` reach `SqlLowerer`. Full dialect support (sqlparser-rs parse + DF plan conversion) remains deferred. |
| ✅ done | `lower/sql.rs` — LIKE / ILIKE: `Expr::Like { case_insensitive }` → `Compare { ILike / Like }` |
| ✅ done | `lower/sql.rs` — arithmetic: `Operator::Plus/Minus/Multiply/Divide/Modulo` → `L3Expr::Arith` |
| ✅ done | `lower/sql.rs` — unary minus: `Expr::Negative` → negate literal or wrap in `Arith(Mul, -1, x)` |
| ✅ done | `lower/sql.rs` — CASE: `Expr::Case` → `L3Expr::Case` |
| ✅ done | `lower/sql.rs` — UNION: `LogicalPlan::Union` → left-associative `SetOp { Union, all: true }`; UNION DISTINCT handled by existing Distinct arm |
| ✅ done | `lower/schema_pass.rs` — `populate_schemas(expr, catalog) -> Rc<L3Node>` bottom-up schema pass |
| ⬜ deferred | CTEs (`WITH … AS …`) — lower to `QueryExpr::LetBinding`. DataFusion inlines or wraps in `LogicalPlan::Recursive`. |
| ⬜ deferred | Subqueries / inline views — `FROM (SELECT …) AS alias` currently returns `UnsupportedFeature`. |

---

## 3. Core type system — schema derivation

`HasSchema::output_schema` on `QueryExpr` is the full schema-derivation pass.
Node-by-node status:

- ✅ `Scan` — reads columns from `SchemaCatalog`; sets `time_index` from `time_column`
- ✅ `Filter`, `Sort`, `Limit`, `Distinct`, `Partition`, `TimeWindow` — pass-through child schema
- ✅ `Aggregate` — `by` columns + one output column per `AggIntent`; TopK special-cased (by-cols + synthetic `count`)
- ✅ `Project` — Column items look up field in child schema (alias renames); time_index tracks the time col through reordering and aliasing. Non-column exprs (`Cast`, `FunctionCall`) remain `todo!()` until type inference exists.
- ✅ `Merge` — pass-through first child schema (all shards share the same schema)
- ✅ `WindowFunc` — child schema + one appended column; type derived from `WindowFuncKind` and `args: Vec<L3Expr>` (added to IR); ranking funcs → `Int64` not-nullable; nav funcs (`Lag`/`Lead`/etc.) → arg type, nullable; `Min`/`Max` → preserve arg type
- ✅ `SetOp` — left schema (UNION / INTERSECT / EXCEPT output is left-shaped); `time_index` propagated from left
- ⬜ `Join` — merge left + right schemas; handle column-name collisions with table-qualified names (deferred until JOIN lowering is implemented)
- ⬜ `BinaryOp`, `LetBinding`, `Subquery`, `Ref` — blocked on those query paths existing

---

## 4. L4 — sketch binding (not started)

The entire L4 layer (`SummaryExpr` types exist in `core` but nothing produces
them yet).

### Rule engine
A fixed-point rewrite engine that walks an L3 `QueryExpr` DAG, matches rules,
and emits an L4 `SummaryExpr` DAG. Core should own the engine; deployment
models inject rule sets.

### Bind rules (one per AggIntent × SummaryKind pair)
Each rule matches a specific `AggIntent` variant and, given
`DeploymentConstraints` and an `AccuracyTarget`, selects a `SummaryKind` +
`SummaryParams`. Minimum set for the SQL path:

| AggIntent | Candidate SummaryKind |
|---|---|
| `Count` | `Count` (exact), `Cms` (approx) |
| `Sum` | `Sum` (exact), `Cms` (approx) |
| `Min` / `Max` | `MinMax` (exact), `Kll` (approx) |
| `Avg` | `Sum` + `Count` pair, or `Kll` |
| `Stddev` | exact accumulator (Welford) — no sketch analog today |
| `Quantile` | `Kll`, `DDSketch` |
| `Cardinality` | `Hll` |
| `TopK` | `CmsWithHeap` |

### Cost model trait
Trait `CostModel` with `plan_cost(plan: &SummaryExpr, constraints: &DeploymentConstraints) -> Cost`.
`Cost` should carry accuracy estimate, latency estimate, and transmission bytes.
L4 uses cost to pick among bind-rule alternatives.

### `DeploymentConstraints`
Input to L4: memory budget per stage, sketch catalogue (which
`SummaryKind`s are available in this deployment), topology (number of stages).
Currently not defined anywhere.

### Schema derivation for L4
`L4Node.schema` is always empty today (same as L3). Implement
`HasL4Schema` — `SummaryAgg` emits a `Sketch(kind, params)` column;
`SummaryEstimate` collapses it back to a primitive column.

---

## 5. L5 — stage allocation and emission (not started)

### Stage allocator
Colors the L4 DAG by `StageId`. For the SQL path (single-stage), this is
trivial: assign every node `StageId(0)`. For multi-stage deployments (DC's
3-stage topology), this is the main algorithm.

### `PlanEmitter` trait
Converts a stage-allocated L4 DAG to an output format. Needed
implementations per deployment model:
- `OpAMP RemoteConfig` YAML (asap-lifecycle)
- `StreamingConfig` POST body (asap-query)
- Rewritten DataFusion `LogicalPlan` (asap-fusion)

---

## 6. PromQL lowering path (not started)

`QueryLanguage::PromQL` workloads have no lowerer. Per the migration plan
(Phase 4), this requires:

1. Define `PromqlLogicalPlan` (L2 tree) in `core::logical_plan::promql` —
   five pattern shapes from asap-planner-rs as first-class nodes.
2. Implement `PromqlLowerer`: promql-parser AST → `PromqlLogicalPlan` (L1→L2).
3. Implement L2→L3: `PromqlLogicalPlan` → `QueryExpr` with `Source::TimeSeries`
   leaves, `AggIntent::Rate` / `AggIntent::Increase` for counter-based metrics.
4. Add PromQL tests mirroring the SQL test suite.

---

## 7. Deployment model crates (not started, per migration plan)

| Crate | Phase | Scope |
|---|---|---|
| `deployment-model-asapfusion` | 3 | Thin model: picks core L4 rules, DataFusion emitter |
| `deployment-model-asapquery` | 4 | Migrates asap-planner-rs; adds PromQL L2 tree |
| `deployment-model-asaplifecycle` | 5 | DC-specific cost models, OpAMP emitter, 3-stage topology |

---

## 8. Infrastructure / integration

- **`cargo clippy` clean pass** — ✅ done for `lower` crate; dead-code warnings on stub types in `core` remain until those paths are implemented.
- **HTTP entry point** — no server exists yet; `lower_batch` is a library function with no HTTP handler wired up.
- **End-to-end test** — no test goes from a raw SQL string all the way to a `SummaryExpr` tree; blocked on L4 existing.
- **Benchmarks** — no criterion benchmarks for the lowering path; add at least one for `lower_batch` over a realistic query corpus. Unblocked now.
- **Phase 7 cleanup** — once all three deployment models land, revisit `todo!()`s and `#[allow(unused)]` in Phase 1 trait stubs.
