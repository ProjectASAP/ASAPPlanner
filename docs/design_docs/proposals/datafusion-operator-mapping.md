# Reusing DataFusion Operator Implementations

> Status: proposal, not implemented. Audience: executor designers.
> Checked against `asap-executor` on `main` and DataFusion 57.

## 1. Context

`asap-executor` (`crates/executor`) executes the Planner's physical DAG over
Arrow batches, for both query-time and ingestion-time computation. It contains:

- **Operators:** relational (filter, aggregate, sort, join), PromQL (range
  functions, vector matching), and **summary** operators. A summary is a
  per-group state, either a sketch (KLL, DDSketch, HLL, CMS) or an exact
  state (sum, count), that is built, merged and evaluated.
- **Runtime:** schedules operators over the DAG and passes one node's output
  to each of its consumers.

Today operator logic and runtime are fused: every operator implements
`PhysicalOperator::start(input streams) -> output stream` and runs its own
loop, with memory reservations, cooperative yields and cancellation.

**Goal:** first, minimize implementation and maintenance effort by reusing
[DataFusion](https://github.com/apache/datafusion)'s operator code; second,
efficiency. There is no executor benchmark yet. Five operator files
(`group_table`, `joins`, `time_range`, `range_function`, `series_labels`)
convert batches to rows (`Batch::to_rows`), the likely hot spots.


## 2. Decisions

1. **Operators are plain logic, driven by our runtime:**

   ```rust
   trait OperatorLogic {
       fn push(&mut self, input: usize, batch: Batch) -> Result<Vec<Batch>>;
       fn advance(&mut self, t: Timestamp) -> Result<Vec<Batch>>; // emit windows closed before t
       fn finish(&mut self, input: usize) -> Result<Vec<Batch>>;
   }
   ```

   One interface serves both modes; only the driver differs:

   - Batch (query): `push` every batch, then `finish`.
   - Streaming / mini-batch (ingestion): `push` batches as they arrive, and
     `advance(t)` when time passes a window boundary.

   This replaces a first proposal of two interfaces,
   `compute(batch) -> batch` for batch and `init / update / finalize` for
   streaming. `compute` covers only stateless operators: here it is a `push`
   that returns one batch. `init / update / finalize` is the shape of an
   aggregation's state, which §4 keeps and extends.
2. **No reservations, yields or cancellation** in operators.
3. **No DataFusion runtime** (`ExecutionPlan`, `TaskContext`, async
   streams). A DataFusion operator pulls its input from child plans. To run
   `SortExec` on our data we would build a small plan per run:

   ```text
   our batches ──▶ StreamLeaf (an ExecutionPlan we write) ──▶ SortExec ──▶ output stream
   ```

   That buffers all input until `finish`, needs an async driver, and does
   not fit decision 1.
4. **Late data is dropped:** an emitted window is final.
5. **No checkpointing** of operator state for now.
6. **First step:** split operator logic from the runtime (decision 1)
   before moving operators onto DataFusion.
7. **No encoding of summary states between operators.** `push` takes our
   `Batch`: a `RecordBatch` plus in-memory sketch states (`StateColumn`).
   Wrapped DataFusion components receive its `RecordBatch`. Our sketch
   accumulators hand states to the next operator in memory, through our own
   methods beside `GroupsAccumulator`; `state` (encoded Arrow arrays) is used
   only to store a state.

## 3. Wrap, copy or implement

- **Wrap:** call a public DataFusion component that needs no DataFusion
  runtime. No measurable overhead: it is a direct call on the `RecordBatch`
  our `Batch` already holds.
- **Copy:** port DataFusion code into our crate, for logic that is either
  private (`TopK`) or reachable only through an `ExecutionPlan` (hash join
  probing). We keep DataFusion's tested algorithm without its runtime, but
  maintain the copy: keep its Apache-2.0 header, list it in
  `THIRD_PARTY.md`, and record the source commit.
- **Implement:** our own code, where DataFusion has no equivalent or the
  operator is trivial.

## 4. Grouped state with `GroupsAccumulator`

Every grouped operator (aggregate, summary build, summary merge) needs
per-group state. Example: a KLL sketch of `latency` per `job`:

```text
job   latency
api   12
db    40
api   15
```

- **Today:** one sketch object per group, updated one row at a time. Our
  `group_table` finds each row's group, then
  `updater[group].update_single(latency)` runs once per row.
- **`GroupValues` (DataFusion):** maps a batch's key columns to a group id
  per row: `[api, db, api]` → `[0, 1, 0]`.
- **`GroupsAccumulator` (DataFusion):** one object holding every group's
  state, updated a batch at a time:
  - `update_batch([12, 40, 15], group_ids = [0, 1, 0])`: sketch 0 gets 12
    and 15, sketch 1 gets 40.
  - `evaluate(EmitTo::All)`: one value per group, e.g. p99 for `api` and
    for `db`.
  - `state(...)`: each group's state as Arrow arrays, to store or merge.
  - `merge_batch(states, group_ids)`: merges states into groups.

`init / update / finalize` maps to creating the accumulator, `update_batch`
and `evaluate`. Two additions matter for ASAP. `merge_batch` combines
tumbling-window states into longer windows. `state` serializes a state for
storage. A `retract` (remove values, for sliding windows) is not used,
because most sketches (KLL, HLL) cannot remove values; sliding windows
merge tumbling-window states instead.

**Proposal:** each sketch family and each PromQL aggregate implements
DataFusion's `GroupsAccumulator` trait; SQL aggregates use DataFusion's
built-ins. Consequences:

- `push` = `GroupValues` + `update_batch` (or `merge_batch`); `finish` =
  `evaluate`.
- `advance` keeps one `GroupValues` + accumulator set **per open window** and
  emits and drops a window's set when it closes. DataFusion's own partial
  emit (`EmitTo::First(n)`) does not work here, because group ids follow
  arrival order, not window order.
- `evaluate` and `state` reset what they emit. A sliding window reads the
  same tumbling-window state several times, so our accumulators implement
  the non-resetting `evaluate_preserving` / `state_preserving` (DataFusion
  PR #24714).
- HydraCms (one state shared by all groups) does not fit and stays ours.

## 5. Per-operator mapping

Operators are in `crates/executor/src/operators/`.

| Operator | How | Detail |
| --- | --- | --- |
| `project`, `filter`, scalar `Expression` | Wrap | Expressions are lowered to DataFusion `PhysicalExpr`s; `filter` then applies Arrow's `filter` kernel. PromQL label expressions (`LabelSet`, `Label`, `LabelIdentity`) have no DataFusion equivalent and become our own `PhysicalExpr`s. |
| `aggregate` | Wrap + implement | Grouping uses `GroupValues`, and SQL aggregates use DataFusion's accumulators. PromQL `avg`, `quantile`, compensated `sum` and the NaN rules of `min`/`max` differ from DataFusion's, so they are our own `GroupsAccumulator`s. |
| `sql_window` | Wrap | SQL window functions use DataFusion's built-in implementations (`PartitionEvaluator`), whose semantics `sql_window` already follows. |
| `sort` (grouped, optional top-k) | Copy | Sorting uses Arrow's `lexsort_to_indices` and `take`. Top-k is copied from DataFusion's `TopK`, which is private. |
| `relational_join`, `semi_join` | Wrap + copy | The build side uses DataFusion's public `JoinHashMap`. The probe and the output of each join kind (inner, outer, semi, anti, non-equality predicates) are copied from `HashJoinExec`. |
| `certified_semi_join`, `set_operation` | Implement | Both are built on the copied join: the first adds a check that the right side is complete, the second adds a distinct, as DataFusion plans INTERSECT and EXCEPT. |
| `union`, `limit`, `scalar`, `source`, `absent`, `vector_to_scalar`, `bind_scalar_ref` | Implement | These are a few lines each on Arrow's `concat` and `slice`, so reuse saves nothing. |
| `per_group_summary_build`, `per_group_keyed_summary_build`, `summary_merge` | Wrap + implement | Grouping uses `GroupValues`; each sketch family is our own `GroupsAccumulator` (§4). |
| `all_groups_summary_build` | Implement | HydraCms keeps one state shared by all groups, which `GroupsAccumulator` cannot express. |
| `evaluation`, `keyed_evaluation` | Implement | They read a value (a quantile, a count) from a summary state, which is sketch-specific. |
| PromQL and time: `time_range`, `evaluate_at`, `scope_timestamp`, `current_series`, `series_window`, `range_window`, `window`, `*_binary`, `series_labels`, `series_relabel`, `series_sample`, `*histogram_quantile` | Implement | DataFusion has no PromQL or time-series operators. Their grouping and label matching can use `GroupValues` and Arrow's `RowConverter`. |

## 6. Risks

- **Semantics.** PromQL differs from DataFusion (NaN ordering, compensated
  sums, overflow as an error). Wrapped and copied operators must pass our
  existing tests unchanged.
- **One producer, several consumers.** DataFusion streams and accumulators
  are boxed and not `Clone`, while our DAG has nodes with several consumers.
  Streams do not matter: we do not use `ExecutionPlan`, and our runtime
  clones batches (cheap, since arrays are `Arc`s) to each consumer.
  Accumulator state does: see the non-resetting reads in §4.
- **Version churn.** These are low-level DataFusion APIs. Pin the version.


## 7. Open questions

Each question lists the options and a recommendation.

**Q1. What does `t` in `advance(t)` mean, and who produces it?**

- (a) The end of the ingestion window that just completed. The runtime
  already knows it: today "end of input" means "window complete".
- (b) An event-time watermark computed from the data.

Recommendation: (a) now; (b) only if out-of-order data within a window
becomes a problem. Either way, the runtime advances operators in DAG order,
delivering each operator's output downstream before advancing the next.

**Q2. Where does an operator get the evaluation time, now that it no longer
receives a `RunContext`?**

- (a) As an argument to every `push` and `finish`.
- (b) Split each operator into an immutable definition (kept in the compiled
  plan) and per-run state created by `definition.open(ctx)`, where `ctx`
  holds the evaluation time.

Recommendation: (b). `OperatorLogic` holds mutable state, so each run needs a
fresh instance anyway, and `open` is where it is created. `evaluate_at` (`@`)
stays a runtime action: the runtime opens its inputs with a shifted `ctx`,
as `input_context` does today.

**Q3. Which operators must support `advance` (streaming) in the first
version?**

- (a) Only those that ingestion-time plans use today (to be listed from the
  physical compiler); every other operator is batch-only, and the runtime
  rejects a streaming plan containing one.
- (b) All operators.

Recommendation: (a). Sort and join can only emit at `finish`, and PromQL
operators such as `series_window` and `current_series` run only at query
time.

**Q4. How do two-input operators (joins, PromQL binary operators) stream?**

- (a) They do not: batch-only in the first version.
- (b) Define per operator what `push` buffers and when it emits; e.g. a
  binary operator emits a window once both inputs have advanced past it.

Recommendation: (a), unless the answer to Q3 lists one of them.

**Q5. In what order do operators move to the new interface?**

Recommendation: write one generic driver that runs an `OperatorLogic` as
today's `PhysicalOperator`, so operators move one at a time and the existing
executor tests check each move. Order: stateless operators, then aggregates
and summaries, then sort and join, then PromQL operators.

**Q6. How much of `Expression::Planner` (the Planner's full scalar IR) can be
lowered to DataFusion's `PhysicalExpr` and built-in functions?**

Unchecked. Needs a pass over the scalar IR before migrating `project` and
`filter`.

**Q7. Depend on `datafusion-physical-plan` for `GroupValues`, or copy
`GroupValues`?**

The crate pulls in DataFusion's runtime crates at compile time, unused.
Recommendation: depend on it first; copy only if build time or version
conflicts hurt.
