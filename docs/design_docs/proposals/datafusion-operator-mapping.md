# Reusing DataFusion Operator Implementations

> Status: proposal, not implemented. Audience: executor designers.
> Checked against `asap-executor` on `main`, DataFusion `main` (57.0.0-dev)
> and the DataFusion 54.2.0 release.

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
2. **No memory accounting, cooperative yields or cancellation**, in
   operators or in the runtime. The runtime's `MemoryLimit` failure and run
   cancellation are removed. The runtime stays async only because store reads
   are async; operators are synchronous calls.
3. **No DataFusion execution layer.** By "DataFusion's runtime" this doc means
   the machinery DataFusion's own operators run on: the `ExecutionPlan`
   trait, `TaskContext` and `RuntimeEnv` (memory pool, spill files), async
   record-batch streams, and tokio. Every DataFusion `*Exec` operator is
   built on it. For example, `SortExec` (`physical-plan/src/sorts/sort.rs`)
   pulls its input by calling `execute()` on a child plan, reserves memory
   from the `MemoryPool`, spills to disk through `RuntimeEnv`, and uses tokio
   for spill I/O. Running a `*Exec` on our data (rejected) would need a
   small DataFusion plan per run:

   ```text
   our batches ──▶ StreamLeaf (an ExecutionPlan we write) ──▶ SortExec ──▶ output stream
   ```

   That buffers all input until `finish`, needs an async driver, and brings
   back the memory accounting decision 2 removes. So we never run a
   `*Exec`: we call only DataFusion components that do not need this layer
   ("wrap", §3), and copy logic that is only available inside a `*Exec`
   ("copy"), e.g. sort and the hash-join probe (§5).
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
8. **`advance(t)`: `t` is the end of the ingestion window that just
   completed**, which the runtime already knows. Event-time watermarks only
   if out-of-order data within a window becomes a problem. The runtime
   advances operators in DAG order, delivering each operator's output
   downstream before advancing the next.
9. **Evaluation time enters at `open`.** Each operator is an immutable
   definition, kept in the compiled plan, and per-run state created by
   `definition.open(ctx)`, where `ctx` holds the evaluation time. `@`
   (`evaluate_at`) stays a runtime action: the runtime opens the operator's
   inputs with a shifted `ctx`.
10. **Every operator can run at ingestion time in per-window batch mode;
    only the tumbling-window chain's operators support `advance`** (proposed,
    not yet agreed). The planner places a node at ingestion time when its
    result does not depend on the evaluation time, not by operator kind, so
    almost any operator, including joins and `aligned_binary`, can reach an
    ingestion plan (§8). Per-window batch mode is what runs today: a fresh
    instance per ingestion window, `push` all, `finish`. `advance` is needed
    only to keep state across windows; in the first version it is supported
    by `filter`, `project`, `time_range`, `per_group_summary_build` and
    `window_end_timestamp`, which make up every ingestion-time chain in the
    corpus snapshots. Two-input operators stay per-window batch.
11. **Migration is incremental.** A generic driver runs any `OperatorLogic`
    as today's `PhysicalOperator`, so operators move one at a time and the
    existing executor tests check each move. Order: stateless operators,
    aggregates and summaries, sort and join, PromQL operators.
12. **Depend on `datafusion-physical-plan` for `GroupValues`**, although it
    pulls in DataFusion's runtime crates at compile time. Copy `GroupValues`
    only if build time or version conflicts hurt.
13. **Pin DataFusion 54.2.0.** It is the newest release on our Arrow major
    version (58), so no Arrow upgrade is needed. Releases and their Arrow
    versions: 54.x → 58, 55.x → 59, 56 (release candidate, Nov 2026) → 60.
    The APIs this doc uses are public in 54.2.0; `TopK` and `ExternalSorter`
    are private there too.

## 3. Wrap, copy or implement

- **Wrap:** call a public DataFusion component that needs no DataFusion
  runtime (never a `*Exec`). No measurable overhead: it is a direct call on the `RecordBatch`
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
  same tumbling-window state several times, so our sketch accumulators also
  provide non-resetting reads. Our operators call these directly, so we do
  not need DataFusion's own version of them (`evaluate_preserving` /
  `state_preserving`, first released in DataFusion 56).
- HydraCms (one state shared by all groups) does not fit and stays ours.

## 5. Per-operator mapping

Operators are in `crates/executor/src/operators/`.

| Operator | How | Detail |
| --- | --- | --- |
| `project`, `filter`, scalar `Expression` | Wrap + implement | Expressions are lowered to DataFusion `PhysicalExpr`s; `filter` then applies Arrow's `filter` kernel. About half of our expression forms map directly, a third need care when lowering, and the rest are ours (§9). |
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
- **Arrow version.** DataFusion and our crate must use the same Arrow major
  version (decision 13). Each DataFusion upgrade may force an Arrow upgrade,
  which reaches every deployment that builds against `asap-executor`.

## 7. Open questions

None open. §8 and §9 record the investigations that answered them.

## 8. Finding: operators at ingestion time

- **Placement rule.** A node may run at ingestion time if its result does
  not depend on the evaluation time (no `TimeRange` relative to it, no
  `now()`, `time()` or `@ start()`/`@ end()`), or if it feeds such a node
  (`ingestion_time_offered_per_group` in
  `crates/stage2-physical-optimization/src/group.rs`). Stage 2 does not
  filter by operator kind.
- **Observed.** Every ingestion-time node in the corpus snapshots is a
  `Scan` or a tumbling-window summary chain. A chain lowers to `time_range`
  → `per_group_summary_build` → `window_end_timestamp`, with `filter` and
  `project` between scan and chain.
- **Possible but not observed.** Any operator over time-independent input:
  aggregates, sort, SQL joins and set operations without `now()`, `union`.
  `aligned_binary` is the ingestion-time lowering of `BinaryOp`
  (`physical_planner/mod.rs`); `series_binary` and `vector_binary` are
  query-time only.

## 9. Finding: expressions on DataFusion `PhysicalExpr`

Each expression form of our `Expression` enum and the Planner scalar IR was
classified against DataFusion.

- **Same semantics (about half).** Columns, literals, the evaluation time
  (bound as a literal), Boolean and NULL logic, `LIKE`, searched `CASE`,
  Float64 arithmetic, casts (ours already follow DataFusion's), struct
  field access, and most PromQL math functions (`abs`, `ln`, `sqrt`, trig,
  `sgn`, `deg`, `rad`). PromQL `=~` / `!~` also match once the pattern is
  anchored (`^(?s:p)$`) when lowering.
- **Different semantics (about a third).** The causes repeat:
  - *Float comparison.* Arrow compares floats by IEEE total order: NaN
    equals NaN and is greater than every number, and -0.0 < 0.0. We use
    IEEE comparison: any comparison with NaN is false. This affects every
    comparison, `IN` and `CASE x WHEN`. One wrapper around comparisons
    fixes it.
  - *Integer overflow.* DataFusion wraps unless
    `BinaryExpr::with_fail_on_overflow(true)` is set, and negation always
    wraps. We raise an error.
  - *Int64 vs Float64 comparison.* DataFusion casts the integer to float and
    loses precision; we compare exactly.
  - *Missing map key or list element.* DataFusion returns NULL; we return
    the type's default when the value is non-nullable.
  - *PromQL functions.* `clamp` must return NaN for NaN input, `round` takes
    a "to nearest" step, and the date functions take float seconds. Each is
    a short composition of built-ins or a small UDF.
- **No equivalent (ours).** PromQL label operations (`LabelSet`,
  `LabelIdentity`, `drop_metric_name`), PromQL float-to-text formatting,
  the checks that fail a query (`FiniteFloat64`, checked division, exact
  Int64→Float64), `mapconcat` and `days_in_month`.
