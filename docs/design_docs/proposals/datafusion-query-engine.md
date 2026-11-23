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

The executor stays until the new crate compiles the same plans. Then `plan_installable`'s compile check (`check_compiles`) switches to the new crate, and the executor is deleted.

- **Kept from the executor:** the sketch kernels, the PromQL semantics code, and the test cases.
- **Why not rewrite in place:** the executor's core abstractions are exactly what this replaces: `Batch` with its state columns, `PhysicalOperator::start`, and its single-threaded streams.
- **The executor remains useful** as an untrusted second answer to compare against during the switch-over.

### D2. The query interface takes a batch of queries

- **A batch is a list of `(root, evaluation)` pairs.** An evaluation is an instant time T or a range `(start, end, step)`. The pairs in one batch may have different evaluations.
- **Sharing is by (node, T).** A node's output depends only on its definition and T, so sharing is correct whatever evaluation grid the queries came from. The engine takes the union of the batch's evaluation times, and computes each needed `OneEvaluation` node once for all of them.
- **No cache across batches.** Grouping queries that are due at the same time into one batch is the caller's job.
- **Results are per query.** If a needed view is not ready, only the queries that need it fail with "not ready" and go to the fallback, as in ASAPQuery's `Ok(None)` today.

### D3. Cut the DAG at kept nodes, then run the trees in dependency order

- **Cutting:** the DAG is cut at every `OneEvaluation` and `AcrossEvaluations` node. Each piece is a tree and becomes one DataFusion plan.
- **Refusal:** a plan with a query-time `Lifetime::None` node that has two consumers is refused.
- **Running a batch:**
  - Trees run one at a time, in dependency order: a tree starts only after every tree it reads has finished. Running independent trees concurrently is a later optimization.
  - A kept output is collected fully into a per-batch scratch store, re-packed into normal-sized batches.
  - Root results are streamed to the caller.
  - A failed tree fails only the queries that depend on it.
- **The scratch store and the sketch registry (D5) live for the whole batch.**
- **Not done here:** a fan-out operator that streams one output to several parents, such as a `SharedExec` with per-consumer buffers. It can deadlock, for example when both sides of one join read the same upstream, and with a spillable buffer it amounts to materialization anyway. It may come later as an optimization for large `OneEvaluation` outputs.

### D4. Store interfaces: two traits, defined by this crate and implemented by the deployment

- **`ViewStore::read(definition_id, selections) -> ViewBatch | NotReady`**
  - Views are keyed by the plan's view `definition_id` plus a selection: the label constraints, the pane width and the window end. This is the keying ASAPQuery-backend's `SummaryStore` used.
  - Reads are **exact**: a missing window means `NotReady`, never a silently shorter answer. ASAPQuery's `Store` returns sparse results today, so it cannot tell "no data" from "not built yet".
  - `ViewBatch` holds Arrow rows plus `Vec<Arc<dyn Sketch>>`.
- **`ViewStore::write(...)`** is only for `OnFirstRead` views, which this engine computes. It is append-only, and the store applies retention.
- **`RawDataStore::read(source, time_range, columns, predicate_hint)`** returns Arrow record batches. The predicates are a hint; the engine always re-filters.
  - ASAPQuery stores no raw data, so its first implementation fetches from Prometheus, or ClickHouse for SQL, the way ASAPQuery-backend did.
- **Not DataFusion's `TableProvider`:** plans are compiled once but bound to the stores per batch, and view rows carry sketch objects. The leaf operators look up the store handles from the session context when they run.
- **No snapshot:** a stored window never changes once written. So a read either sees it or doesn't. If a window is evicted mid-batch, the queries that read it get `NotReady`.

### D5. Sketch states as handles; exact states as plain columns

- **Sketches:** the executor's `AggregateCore` kernels are ported as the `Sketch` trait. `merge_with` returns a new object, so shared sketches are never changed in place.
- **Inside a batch:** a sketch column is a `UInt64` column of ids into a per-batch, append-only registry of `Arc<dyn Sketch>`.
  - The column's Arrow field metadata records the sketch family and parameters.
  - The compiler checks that merge and estimate inputs have the same family and parameters.
  - The registry reserves memory from DataFusion's memory pool, counting a shared Hydra grid once.
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
- **The existing endpoints stay.** `/api/v1/query` and `/query_range` become batches of one, for Grafana and other clients. Sharing across queries happens only through the batch endpoint.
- **Matching a query to a root:** at install, each of `query_texts[i]` is parsed with ASAPPlanner's frontend and printed back in a fixed canonical form, mapping to `roots[i]`. Incoming queries are canonicalized the same way and looked up. Matching by what a query can be answered from, rather than its text, is a separate later feature.
- **Fallback per entry:** a query with no matching root, or one that gets `NotReady`, is sent to the fallback (Prometheus or ClickHouse) on its own, and its answer fills that query's slot.
- **SQL** follows the same path with the SQL frontend, after PromQL works.

### D11. Labels are plain string columns

The planner's schemas already fix the logical shape: one column per label the query uses, plus `$promql_series_identity`, which holds the series' complete label set as canonical JSON (`crates/types/src/ir/schema/mod.rs:278`). Both are plain Arrow string columns.

- **Computing an identity is a function over strings:** parse the JSON, change the labels, and serialize again. `without` grouping, relabeling and binary operations that drop `__name__` use it.
- **Later optimizations:** dictionary encoding, sharing bytes between repeated `Utf8View` values, or interned `u64` series ids.

## Open questions

These are not yet discussed:

- How each operator maps to a DataFusion `*Exec`, a function, or a custom operator. The `docs/datafusion-operator-mapping` §9 table is the starting point.
- Testing: comparing answers with Prometheus over the same raw data, and with the executor during the switch-over.
- Order of implementation.
