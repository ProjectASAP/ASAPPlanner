# DataFusion query-time engine

**Status:** Design proposal, in discussion. A decision log, not yet reviewed by collaborators.

**Audience:** Planner and data-plane designers and implementers.

ASAPPlanner emits an `InstallablePlan`: a physical DAG (`PhysicalASAPDAG`) plus query texts. We will execute the query-time part of that DAG on Apache DataFusion, in a new crate in this repository. The new engine replaces `crates/executor` and the query engine inside ASAPQuery's `asap-query-engine`. It does not replace ASAPQuery's precompute engine: that engine is changed separately to build the plan's ingestion-time views, and this engine assumes the views get built.

## Context

- **`crates/executor` is being replaced.** It has a long list of wrong-answer and missing-semantics bugs (`BUG-COVERAGE.md`; issues #1336, #1311, #1345, #1310, …). Its single-threaded, one-evaluation-time-per-run design is what we replace, not its sketch kernels.
- **DataFusion plans are trees.** The ASAP plan is a DAG: a node can have several consumers, both within a query and across queries.
- **The plan already says where nodes are shared.** At query time, a node with several consumers is one of:
  - a leaf: a raw `Scan` or a read of a materialized view;
  - a node with `Lifetime::OneEvaluation`;
  - a node with `Lifetime::AcrossEvaluations`.

  Stage 3 never gives a query-time `Lifetime::None` node a second consumer (executor README).
- **Earlier work this builds on:**
  - **sidra** materializes shared subplans into a store that consumers then read. Its notes on shared steps that cost more than they save, and on tiny batches, apply here.
  - The **`docs/datafusion-operator-mapping` branch** maps each plan operator to a DataFusion building block, but keeps the executor's own runtime. This proposal instead runs DataFusion's `ExecutionPlan`s.

## Decisions

**Principle:** build the simplest correct version first. Performance work, such as dictionary encoding, interned ids, a fan-out operator and sliding pane merges, comes later and is driven by measurements.

### D1. A new crate next to `crates/executor`

The crate is `crates/query-time-batch-engine`, package `asap-query-time-batch-engine`.


The executor stays until the new crate compiles the same plans. Then `plan_installable`'s compile check (`check_compiles`) switches to the new crate, and the executor is deleted.

- **Kept from the executor:** the sketch kernels, the PromQL semantics code, and the test cases.
- **Why not rewrite in place:** the executor's core abstractions are exactly what this replaces: `Batch` with its state columns, `PhysicalOperator::start`, and its single-threaded streams.

### D2. The query interface takes a batch of queries

- **A batch is a list of `(root, evaluation)` pairs.** An evaluation is an instant time T or a range `(start, end, step)`. The pairs in one batch may have different evaluations.
- **Sharing is by (node, T).** A node's output depends only on its definition and T, so sharing is correct whatever evaluation grid the queries came from. The engine takes the union of the batch's evaluation times, and computes each needed `OneEvaluation` node once for all of them.
- **Grouping queries that are due at the same time into one batch is the caller's job.**
- **Results are per query.** If a needed view is not ready, only the queries that need it fail with "not ready" and go to the fallback, as in ASAPQuery's `Ok(None)` today.

### D3. Cut the DAG at kept nodes, then run the trees in dependency order

A **kept node** is a node whose output the plan says to keep, not just stream to its one consumer:
- `Lifetime::OneEvaluation`: kept for one evaluation, so every query evaluated at the same time can read it.
- `Lifetime::AcrossEvaluations`: a view, kept in the view store across evaluations (D4).

At query time, these are the only computed nodes that can have more than one consumer.

- **Cutting:** the DAG is cut at every kept node. Each piece is a tree and becomes one DataFusion plan.
- **Refusal:** a plan with a query-time `Lifetime::None` node that has two consumers is refused.
- **Running a batch:**
  - Trees run one at a time, in dependency order: a tree starts only after every tree it reads has finished. Running independent trees concurrently is a later optimization.
  - A kept output is collected fully into the batch's scratch store, re-packed into normal-sized batches.
  - Root results are streamed to the caller.
  - A failed tree fails only the queries that depend on it.
- **The scratch store** is an in-memory map, created per batch, from a kept node to its collected Arrow batches for the batch's evaluation times. A leaf operator in each consuming tree reads from it. It is dropped when the batch finishes.
  - It holds every `OneEvaluation` output.
  - For an `AcrossEvaluations` node, the consuming trees read the view store instead (D4). An `OnFirstRead` view that the batch computes is written to the view store if its window is finished (D9). It is also put in the scratch store, so the batch can use windows that are not yet finished, which are never saved.
- **The scratch store and the sketch registry (D5) live for the whole batch.**
- **Not done here:** a fan-out operator that streams one output to several parents, such as a `SharedExec` with per-consumer buffers. It can deadlock, for example when both sides of one join read the same upstream, and with a spillable buffer it amounts to materialization anyway. It may come later as an optimization for large `OneEvaluation` outputs.

### D4. Store interfaces: two traits, defined by this crate and implemented by the deployment

- **`ViewStore::read(view, windows) -> ViewBatch | NotReady`**
  - A view is identified by `(PlanId, PhysicalASAPNodeId)`. `PlanId` is the installed plan's version, and the node fixes the view's sub-DAG, selection, θ and window widths. A stored window is keyed by `(PlanId, PhysicalASAPNodeId, window)`, where `window` is the absolute `(start, end]`. This follows the comment on ProjectASAP/ASAPQuery#935.
  - ASAPPlanner's `definition_id` is not used as a key: it leaves out θ, the window width and the selection, so two different views in one plan can share it.
  - Views cannot be reused across plans. That is fine while ASAPQuery installs one plan (the engine-boundaries doc's I14). Reusing views across plans would need a plan-independent id (ProjectASAP/ASAPPlanner#1429).
  - Reads are **exact**: a missing window means `NotReady`, never a silently shorter answer. ASAPQuery's `Store` returns sparse results today, so it cannot tell "no data" from "not built yet".
  - `ViewBatch` holds Arrow rows plus `Vec<Arc<dyn Sketch>>`.
- **`ViewStore::write(...)`** is only for `OnFirstRead` views, which this engine computes. It is append-only, and the store applies retention.
- **`RawDataStore::read(source, time_range, columns, predicate_hint)`** returns Arrow record batches. The predicates are a hint; the engine always re-filters.
  - ASAPQuery stores no raw data, so its first implementation fetches from Prometheus, or ClickHouse for SQL, the way ASAPQuery-backend did.
- **Not DataFusion's `TableProvider`:** plans are compiled once but bound to the stores per batch, and view rows carry sketch objects. The leaf operators look up the store handles from the session context when they run.
- **No snapshot:** a stored window never changes once written. So a read either sees it or doesn't. If a window is evicted mid-batch, the queries that read it get `NotReady`.

### D5. Sketch states as handles; exact states as plain columns

- **Sketches:** the executor's `AggregateCore` kernels (`crates/executor/src/summary_kernels/`) move into their own crate as the `Sketch` trait, before either engine needs them. The executor, ASAPQuery's ingest engine and this engine all use that crate, so states built at ingestion are read with no conversion, and deleting the executor does not affect ingestion. `merge_with` returns a new object, so shared sketches are never changed in place.
- **Inside a batch:** a sketch column is a `UInt64` column of ids into a per-batch, append-only registry of `Arc<dyn Sketch>`.
  - The column's Arrow field metadata records the sketch family and parameters.
  - The compiler checks that merge and estimate inputs have the same family and parameters.
  - Later: the registry reserves memory from DataFusion's memory pool, counting a shared Hydra grid once. Not needed while batches have no memory limit (D16).
- **Serialization happens only at the store's disk boundary.** In-memory views hand out `Arc`s.
- **Exact states are ordinary Arrow columns**, not handles: sum, count, min, max, increase, rate, and `{sum, count}` for `avg`. DataFusion's own aggregates handle them.
- **Rejected alternative:** serialized bytes everywhere, which would deserialize every input of every merge.

### D6. Build `ExecutionPlan`s directly

- **No `LogicalPlan`, no logical optimizer.** ASAPPlanner already chose the plan's shape, and DataFusion's logical rules would undo it. Sidra had to disable common-subexpression elimination for exactly this reason.
- **Expressions** are built as DataFusion `Expr` and turned into `PhysicalExpr` with `create_physical_expr`, which gives DataFusion's type coercion.
- **Only listed physical rules run**, starting with `CoalesceBatches`.
- **Compile once:** each tree is compiled at install and re-run for each batch.

### D7. Operators handle many evaluation times, through an `eval_time` column

- **`eval_time` is one more key.** Every operator above the first time-dependent operator carries an `eval_time` column:
  - aggregates group by `(eval_time, keys)`;
  - vector matching joins on `eval_time` and the matching labels;
  - per-group sort and limit partition by `eval_time`.

  An instant query is the case with one T.
- **`TimeRange` copies rows by default.** It copies each row once for every evaluation time T whose window covers the row, tagging each copy with `eval_time`. The consumer is then an ordinary aggregate, or a `SummaryMerge`, grouped by `(eval_time, keys)`. For sketch panes, each copy is an 8-byte handle.
- **Range functions are the exception.** `rate`, `increase`, `*_over_time` and similar need each series' time-ordered samples (`rate` handles counter resets and extrapolates to the window edges), so they need a custom operator anyway. That operator is fused with its `TimeRange`: it reads each series' sorted samples once and emits `(eval_time, keys, value)`.
  - Running `TimeRange` alone would copy each row into every T whose window covers it. A 5-minute raw window evaluated every 15 s would copy each sample 20 times.
- **Later optimizations:** fusing `TimeRange` with other consumers, and a sliding merge of panes (two stacks, since sketches can't be subtracted).

### D8. One partition per tree

- **Each tree runs as a single partition.** Most trees are small (an instant query over sketch panes reads a few thousand handle rows), so repartitioning would cost more than it saves. Sidra found the same for its stored results. Parallelism comes from running separate batches concurrently.
- **Later optimization:** split large raw inputs into N partitions by series hash. The range-function operators (D7) work per series, so only the final aggregate would merge partitions.
- **DataFusion's repartitioning rule (`EnforceDistribution`) stays off.** Our compiler decides partitioning.

### D9. Saving `OnFirstRead` views: only finished windows, and the first save wins

`OnFirstRead` is ASAPPlanner's term (`MaterializedViewRefresh::OnFirstRead`). It marks a view that is kept across evaluations but not built at ingestion: the first batch that needs one of its tumbling windows computes that window from its inputs, uses it, and saves it, and later batches read the saved copy. Because the query engine computes these windows, the query engine also writes them.

- **Only finished windows are saved.** `RawDataStore` exposes a *watermark*: the time up to which raw data is complete, after allowing for samples that arrive late.
  - A window that ends at or before the watermark is saved.
  - A newer window is computed for this batch only and not saved, so a later batch recomputes it.
  - Saved windows never change (D4), so saving a window that is still missing samples would make the answer wrong for as long as it is kept.
  - ASAPQuery-backend used the same rule.
  - ASAPQuery's Prometheus-backed raw store needs a watermark; the simplest is "now minus a configured lag".
- **If two batches compute the same window concurrently, the first save wins.** The store ignores the second write. That is safe because the contents of a finished window are fixed. The cost is one duplicate computation.
  - Making the second batch wait for the first is a later optimization, if measurements show it matters.
- **Retention:** the store drops windows older than the view's `retention_ms`, measured from the newest finished window.

### D10. ASAPQuery sends batches through a new endpoint, and matches queries by parsed form

- **New endpoint:** `POST /api/v1/query_batch` takes a list of `{query, time}` or `{query, start, end, step}`. It returns one Prometheus-format result per entry, in order, each marked as answered by the plan, answered by the fallback, or failed.
- **Engine interface:** ASAPQuery's `QueryEngine` trait (engine-boundaries doc) gains `execute_batch`, and `QueryDispatcher` gains `dispatch_batch`; `execute` is a batch of one.
- **The existing endpoints stay.** `/api/v1/query` and `/query_range` become batches of one, for Grafana and other clients. Sharing across queries happens only through the batch endpoint.
- **Matching a query to a root:** at install, each of `query_texts[i]` is parsed with ASAPPlanner's frontend and printed back in a fixed canonical form, mapping to query i's root from `PhysicalASAPDAG::root_nodes_per_query()`. Incoming queries are canonicalized the same way and looked up. Matching by what a query can be answered from, rather than its text, is a separate later feature.
- **Fallback per entry:** a query with no matching root, or one that gets `NotReady`, is sent to the fallback (Prometheus or ClickHouse) on its own, and its answer fills that query's slot.
- **SQL** follows the same path with the SQL frontend, after PromQL works.

### D11. Labels are plain string columns

The planner's schemas already fix the logical shape: one column per label the query uses, plus `$promql_series_identity`, which holds the series' complete label set as canonical JSON (`crates/types/src/ir/schema/mod.rs:278`). Both are plain Arrow string columns.

- **Computing an identity is a function over strings:** parse the JSON, change the labels, and serialize again. `without` grouping, relabeling and binary operations that drop `__name__` use it.
- **Later optimizations:** dictionary encoding, sharing bytes between repeated `Utf8View` values, or interned `u64` series ids.

### D12. Testing

1. **Operator tests** inside the crate, with hand-written inputs and expected outputs. The executor's test cases for the same operators are ported.
2. **Whole-plan tests** inside the crate, over in-memory raw and view stores. A test-only switch treats every `AtIngestion` view as `OnFirstRead`, so the engine builds the views from raw data itself (D9), and any plan from the planner runs in `cargo test` without the precompute engine.
3. **The exact plan is the reference for sketches.** For each query, the planner also produces a plan with only exact states, and the sketch plan's answer must be within the plan's accuracy guarantee of it. This checks the sketch operators only, because both plans run on this engine.
4. **Prometheus is the final reference**, end to end in ASAPQuery, over the same data. Exact plans must match up to float tolerance; sketch plans must be within their accuracy guarantee. This is the gate before switching any traffic.
5. **The old executor is not used as a reference.** Its answers are known to be wrong in places, so a disagreement says nothing about which engine is wrong.

### D13. Operator mapping

Each plan operator becomes a DataFusion `*Exec` with DataFusion's own functions wherever one exists. Our own functions and operators fill only the gaps.

| Plan operator | Becomes |
|---|---|
| `Filter`, `Project`, `Concat`, `Values` | `FilterExec`, `ProjectionExec`, `UnionExec`, an in-memory leaf |
| `Sort`, `Limit` (no grouping) | `SortExec` (with fetch), `GlobalLimitExec` |
| `Sort`, `Limit` per group (e.g. `topk`) | `row_number() OVER (PARTITION BY eval_time, keys ORDER BY …)` in a window operator, then `FilterExec` on `rn <= k` |
| `Join`, `SetOp`, `Dedup` | `HashJoinExec` (semi and anti joins for set operations) and `AggregateExec` for distinct, as DataFusion plans `INTERSECT`/`EXCEPT` |
| `SQLWindowFunc` | DataFusion's window operator |
| `Aggregate` (SQL and PromQL) | `AggregateExec` with DataFusion's functions. PromQL: `sum`, `count`, `avg`, `min`, `max`; `stddev`/`stdvar` → `stddev_pop`/`var_pop`; `quantile` → `percentile_cont`; `count_values` → group by the value, then count; `group` → a constant 1 |
| `SummaryAgg` | `AggregateExec` with a sketch-build aggregate function per family, emitting handles |
| `SummaryMerge` | `UnionExec` of its children, then `AggregateExec` with a merge aggregate function |
| `SummaryEstimate`, `FinalizeExactAccumulator` | `ProjectionExec` with scalar functions |
| `SummaryTumblingWindowChain` (query time) | `window_end = date_bin(w, t − 1 ms, 0) + w` (windows are `(k·w, (k+1)·w]`), then `AggregateExec` grouped by it |
| `TimeRange` | a join of the rows with a table of the batch's evaluation times, keeping a row for T when `T−s−w < t ≤ T−s`. An instant lookback then keeps the latest row per (series, T). |
| `TimeShift` | `ProjectionExec` that shifts the times |
| Range functions (`rate`, `increase`, `*_over_time`, …) | custom operator (D7) |
| `BinaryOp` (vector matching) | custom operator, ported from the executor's `vector_binary.rs`, because PromQL's matching rules (one-to-one, `group_left`/`group_right`, the many-to-many error, dropping `__name__`) are simpler to port than to build out of joins |
| Relabel, info-enrich, series sample, subquery, vector-from-scalar | custom operators or functions |
| `MaintainPopulation`, `EvaluatePopulation`, Hydra's build shared across all groups | custom operators |
| Raw, view and scratch reads | custom leaf operators |

**Known differences from Prometheus**, accepted for now:
- `min`/`max` return NaN if any input is NaN; Prometheus skips NaN unless every input is NaN.
- `topk` ranks NaN first; Prometheus ranks it last.
- `sum`/`avg` do not use compensated (Kahan) summation, so the last digits can differ.

The Prometheus comparison (D12) uses a float tolerance and leaves inputs containing NaN out until these are addressed.

**Risk:** sidra found that DataFusion's cross join emits one batch per left row, and patched it. If the `TimeRange` join hits this, take sidra's patch or write a small custom operator.

### D14. Order of implementation

Each step is a short stack of PRs; each PR compiles and passes tests. Progress is measured by how much of the planner's query corpus compiles and gives correct answers.

| Step | Contents | Ends with |
|---|---|---|
| S1. Skeleton | Crate; cutting and running trees (D3); batch API (D2); store traits with in-memory implementations (D4); `Filter`, `Project`, `Values`, `Concat`, `Sort`, `Limit` over raw reads | hand-built plans running end to end |
| S2. Time | `eval_time`; the `TimeRange` join and instant lookback; `TimeShift`; range functions | `rate(m[5m])`, instant and range, from planner output |
| S3. Exact PromQL | Aggregates (D13); per-group top-k; `BinaryOp`; the remaining PromQL operators; label functions (D11) | the corpus's exact-only PromQL plans |
| S4. ASAPQuery, raw data only | `RawDataStore` over Prometheus; batch endpoint, query matching and fallback (D10); a mode of ASAPQuery's `promql-compliance` harness that plans with ASAPPlanner and runs the new engine | the existing suites comparing against Prometheus |
| S5. Sketches | `Sketch` trait and handle registry (D5); sketch functions; view-read leaf; the test switch that treats every view as `OnFirstRead`; saving `OnFirstRead` views (D9) | sketch plans from the corpus |
| S6. Views from precompute | ASAPQuery's `ViewStore` over the precompute engine's output | depends on the precompute work |
| S7. SQL | joins, set operations, window functions, SQL matching | SQL corpus plans |
| S8. Switch-over | `check_compiles` moves to the new crate; `crates/executor` and the old ASAPQuery query engine are deleted | — |

**Why S4 comes before sketches:** a plan with no views should match Prometheus exactly, apart from D13's known differences. So the existing differential suites strictly check S1–S3's time handling and PromQL semantics, where the executor's bugs were, before sketch error makes every comparison approximate.

### D15. DataFusion version, patches and features

- **The same DataFusion as `crates/frontend-sql`:** 54, which resolves to 54.1.0, with Arrow 58.4. It is pinned exactly in the workspace `Cargo.toml`, and both crates upgrade together.
- **Sidra's patches are not taken up front.** Sidra patches memory accounting for sliced batches in joins and sorts, and the cross join that emits one batch per left row (`sidra/patches/`). A patch is applied through `[patch]` only when a test or benchmark hits its problem. The cross join is the most likely, through `TimeRange` (D13).
- **Features:** default features minus file formats we don't read, such as parquet, avro and compression.

### D16. No resource limits yet; which failures go to the fallback

- **No memory limit, timeout or spilling for now.** A runaway batch can take down the process; that is acceptable while only we run it. Per-batch memory pools and timeouts come later.
- **Only two cases go to the fallback:** a query with no matching root (D10), and `NotReady` (D2). Each one is marked as fallback in the response and counted.
- **Every other error is returned to the caller with no fallback,** so engine bugs stay visible. That covers a failed compile, a kernel error, a type mismatch and an internal error. This is ASAPQuery's existing rule.
- **At install, a plan the engine cannot compile is refused as a whole,** never installed in part. Until S8, the planner's `check_compiles` still uses the old executor, so a plan can pass the planner and still be refused here.

### D17. Standalone scalar queries are out of scope for now

A plan's `scalar_roots` answer queries whose result is one unlabelled number built from other nodes, such as `scalar(sum(rate(errors_total[5m]))) / scalar(sum(rate(requests_total[5m])))`. The final arithmetic (`n2 / n3`) is an expression outside the DAG. The old executor does not answer these queries either.

- **The plan is still installed.** Scalar roots are not compiled, and their queries are not entered in the matching table (D10), so they go to the fallback as unmatched queries. This is the one exception to "refuse a plan that cannot be compiled" (D16).
- **Later:** compute the referenced nodes as kept outputs, then evaluate the expression in one small tree joined on `eval_time`.

## Open questions

None yet: list new ones here as review raises them.
