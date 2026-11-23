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
  - A tree starts only after every tree it reads has finished. Trees that don't depend on each other run concurrently on tokio, up to a cap.
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
- **`TimeRange` (with any `TimeShift`) is fused with its consumer.** The consumer is a range function, an aggregate, or a `SummaryMerge` over panes. The fused operator reads each series' time-sorted rows once and emits `(eval_time, keys, value)`.
  - Running `TimeRange` alone would copy each row into every T whose window covers it. A 5-minute raw window evaluated every 15 s would copy each sample 20 times.
- **Fallback:** a `TimeRange` whose consumer can't be fused uses the copying version and logs a warning.
- **Pane merges:** at first, each T merges the panes in `(T−w, T]`. Sketches can't be subtracted, so a sliding merge (two stacks) is a later optimization.

### D8. One partition per tree, unless the raw input is large

- **Default: each tree runs as a single partition.** Most trees are small (an instant query over sketch panes reads a few thousand handle rows), so repartitioning would cost more than it saves. Sidra found the same for its stored results. Parallelism comes from running independent trees (D3) and separate batches concurrently.
- **Large raw inputs are split by series.** When a raw leaf reports an input above a configurable threshold, it emits N partitions by series hash. The fused windowed operators (D7) work per series, so only the final aggregate merges partitions.
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

## Open questions

These are not yet discussed:

- The shape of ASAPQuery's batch query endpoint, and how incoming queries are matched to plan roots.
- How each operator maps to a DataFusion `*Exec`, a function, or a custom operator. The `docs/datafusion-operator-mapping` §9 table is the starting point.
- Testing: comparing answers with Prometheus over the same raw data, and with the executor during the switch-over.
- Order of implementation.
