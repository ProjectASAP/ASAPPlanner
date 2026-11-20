# Reusing DataFusion Operator Implementations

> Status: proposal, not implemented. Audience: executor designers.
> Checked against `asap-executor` on `main`, DataFusion `main` (57.0.0-dev)
> and the DataFusion 54.2.0 release.

## 1. Goal

`asap-executor` (`crates/executor`) runs the Planner's physical plans over
Arrow data, both at query time and at ingestion time. It implements every
operator itself: filter, aggregate, sort, join, the PromQL operators and the
sketch operators.

Goal: **write and maintain less operator code** by reusing
[DataFusion](https://github.com/apache/datafusion)'s implementations where
they exist. Efficiency is a secondary goal.

How to read this doc: §2 lists what the runtime must do, and §3 defines
terms. §4 shows the code today, §5 the end state, §6 the path from one to
the other, and §7 the decisions behind it. §8 explains the classifications
and the grouped state the decisions rely on. §9–§13 are detail to read once
the overview is clear, and §14 lists the PRs.

## 2. Requirements

What the executor's runtime must do, whichever operator code it runs:

- **Batch and streaming.** A query's input exists already; an ingestion
  window's input arrives over time. Either way, an operator updates its
  state with each mini-batch as it arrives, and keeps no rows it does not
  need: a 10-minute KLL sketch is never built from 10 minutes of buffered
  rows (§7.1).
- **Two-input operators.** The runtime decides the order in which an
  operator's inputs arrive, e.g. a join's build side first (§7.1).
- **Several readers.** One operator's output can feed several operators
  (§10).
- **Single-threaded first.** Version 1 runs one operator at a time; §8.2
  classifies which operators could later be split across threads.
- **No memory accounting, yielding or cancellation** (§7.2).
- **Sketches stay in memory** between operators and are encoded only when
  stored (§7.6).
- **Evaluation time** (a query's time, or an ingestion window) is given to
  an operator when it starts (§7.7).

## 3. Glossary

- **Batch:** a set of rows in columnar form. DataFusion uses Arrow's
  `RecordBatch`. Our `Batch` is a `RecordBatch` plus a list of in-memory
  sketch objects; a summary column of the `RecordBatch` holds, per row, an
  index into that list.
- **Operator:** the computation of one plan node, e.g. "filter rows where
  `x > 5`" or "build a KLL sketch of `latency` per `job`".
- **Runtime:** the code that runs a whole plan: it decides which operator
  runs when, connects each operator's output to the operators that read it,
  and reads data from storage.
- **Driver:** the part of the runtime that feeds one operator its input and
  collects its output, by calling the operator's methods (§5).
- **Query time / ingestion time:** a node runs either when a query is
  evaluated, or as data arrives, ahead of queries.
- **Window:** a fixed time interval, e.g. each 10-minute interval aligned to
  the clock. At ingestion time, each run of a plan covers one window.
- **Mini-batch:** one of the batches in which an operator's input arrives.
- **Batch mode / streaming mode:** in batch mode an operator's whole input
  already exists (e.g. read from storage for a query). In streaming mode it
  arrives over time as mini-batches (e.g. a window's data during
  ingestion). Either way the operator should process each mini-batch as it
  comes, without first collecting all of them (§7.1).
- **Aggregate:** an operator that outputs one final value per group, e.g.
  `sum by (job)`.
- **Summary:** a per-group state output as a column, to be merged or read
  later: a sketch (KLL, DDSketch, HLL, CMS) or an exact state (a running sum
  or count).
- **Accumulator:** the object that holds a group's state (e.g. its sketch)
  and is updated with values.
- **Grouped:** state kept per value of the group-by columns, e.g. one sketch
  per `job` (`by (job)`). A global aggregate is grouped with a single group.
  Streaming systems such as Flink call this "keyed".
- **Keyed (in our code):** not the same as grouped. A *keyed* summary
  (`per_group_keyed_summary_build`, `keyed_evaluation`) is a sketch over
  item keys *within* a group, e.g. a Count-Min sketch of the most frequent
  `path`s per `job`. This doc says "grouped" for per-group state and keeps
  "keyed" for this meaning.
- **Standalone DataFusion code:** public DataFusion functions and types
  that any code can call on a `RecordBatch` or arrays, without the
  execution layer, e.g. `sort_batch`, `PhysicalExpr` (evaluates an
  expression) or `GroupsAccumulator` (§8.3).
- **DataFusion execution layer:** what DataFusion's own operators run on:
  the `ExecutionPlan` trait, `TaskContext` and `RuntimeEnv` (memory pool,
  spill files), async streams and tokio. Each DataFusion operator, named
  `*Exec` (`SortExec`, `HashJoinExec`, `AggregateExec`), is built on it.

## 4. Today

### Our executor

```text
runtime (PhysicalDAG): starts every operator, connects their streams
  │
  └─ each operator: PhysicalOperator::start(input streams) -> output stream
       one function that does everything:
         ├─ loops over input batches
         ├─ memory reservations, yielding, cancellation
         └─ the computation itself, all our own code:
              expressions        our own evaluator (`Expression`)
              grouping           our own `group_table`
              aggregates         our own reductions
              sketches           one sketch object per group, updated one row at a time
              sort, joins        our own
              PromQL operators   our own
```

Two consequences:

- **Computation and running are mixed** in every operator, so no
  computation can be swapped for DataFusion's without rewriting the
  operator.
- **We maintain everything.** Some code also works one row at a time (it
  converts batches to rows with `Batch::to_rows`): `group_table`, `joins`,
  `time_range`, `range_function`, `series_labels` and
  `expressions/columnar.rs`. These are the likely efficiency hot spots
  (a hypothesis; there is no benchmark yet).

### DataFusion

```text
DataFusion
  execution layer   *Exec operators on ExecutionPlan, TaskContext, RuntimeEnv, tokio
                    (SortExec, HashJoinExec, AggregateExec, ...)
  standalone code   PhysicalExpr, GroupValues, GroupsAccumulator,
                    PartitionEvaluator, JoinHashMap, built-in functions and aggregates
  Arrow kernels     filter, take, sort, concat, RowConverter
```

DataFusion's operators (`*Exec`) are tied to its execution layer, just as
ours are tied to our runtime. Its standalone code is not: any code can
call it.

## 5. End state

```text
runtime (ours): runs the plan, connects operators, reads storage
  │
  └─ driver (ours): open → push … push → finish
       │
       └─ operator: only the computation
            ├─ wrap:      our operator calls DataFusion functions (expressions, grouping, SQL aggregates, sort)
            ├─ copy:      DataFusion code ported into our crate (top-k, hash-join probe)
            └─ implement: our own code (PromQL, sketches, trivial operators)
```

The operator interface has two parts: the immutable `Operator` the compiled
plan already holds, and the per-run state it opens. Sources (`Scan`) stay
async streams: reading storage is the runtime's job.

```rust
impl Operator {
    fn open(&self, ctx: &RunContext) -> Box<dyn OperatorLogic>;           // ctx: evaluation time
}
trait OperatorLogic {
    fn push(&mut self, input: usize, batch: Batch) -> Result<Vec<Batch>>;  // one batch of input i
    fn finish(&mut self, input: usize) -> Result<Vec<Batch>>;              // input i has ended
}
```

Example: a KLL sketch of `latency` per `job` over a 10-minute window, whose
data arrives as mini-batches.

```text
Today:     for each row:  find its group in group_table → sketch[group].update(latency)

End state: open(ctx)        new GroupValues + KLL GroupsAccumulator
           push(batch 1)    GroupValues: [api, db, api] → [0, 1, 0]
                            accumulator.update_batch([12, 40, 15], [0, 1, 0])
                            the rows are then dropped; only the sketches remain
           push(batch 2)    same
           …
           finish           emit one sketch per job
```

The same operator runs in batch and streaming mode; only when the driver's
`push` calls happen differs.

## 6. Path from today to the desired end state

Two phases, so that the interface change and the DataFusion change are never
reviewed together. §14 lists the PRs.

- **Phase A: new interface, same internals.** Remove memory accounting,
  yielding and cancellation, then move every operator to `OperatorLogic`
  with its current code, then delete the old path. Existing tests pass
  unchanged.
- **Phase B: DataFusion internals.** Replace one part per PR:
  grouping, accumulators, sort, joins, expressions. Expressions go last
  because they change query results (§7.9).

## 7. Decisions

### 7.1 Separate computation from running

Operators become plain computation (`OperatorLogic`, §5). The runtime and
its drivers own everything else. This is what makes the rest possible:
once an operator is only computation, its computation can come from
DataFusion.

One interface serves batch and streaming mode. The driver calls `push` for
each mini-batch, as soon as it is available, then `finish` when an input
has ended: at the end of a query's input, or at the end of an ingestion
window. An operator that can update its state per mini-batch must do so,
and not keep the rows: a 10-minute KLL sketch is updated with each
mini-batch, never built from 10 minutes of buffered rows. Some operators
cannot avoid keeping rows until `finish` (§8.2). (`compute(batch)` and `init / update / finalize`, an earlier
two-interface idea, are a `push` returning one batch and an accumulator,
§8.3.)

The driver orders the inputs of a two-input operator: a join receives every
build-side batch and `finish` for that input before its first probe-side
batch.

### 7.2 Remove memory accounting, yielding and cancellation

From operators and from the runtime, to keep both simple. The runtime stays
async only because storage reads are async; operator calls are synchronous.
Operators run one at a time; parallel execution is out of scope for the
first version.

### 7.3 Call DataFusion's standalone code, never its execution layer

Running a DataFusion `*Exec` from our runtime would mean building a small
DataFusion plan around our data:

```text
our batches ──▶ StreamLeaf (an ExecutionPlan we write) ──▶ SortExec ──▶ output stream
```

That brings back the execution layer: async pulling, tokio, a memory pool,
spill files. So every operator of ours is built in one of three ways:

- **Wrap:** our operator's `push` and `finish` call DataFusion functions,
  e.g. `sort_batch` or `GroupValues::intern`, on the `RecordBatch` our
  `Batch` already holds, and add only what DataFusion lacks, such as
  keeping rows between calls. These are direct calls, with no measurable
  overhead.
- **Copy:** port DataFusion code into our crate, when the logic exists only
  inside a `*Exec` (the hash-join probe) or is private (`TopK`). We keep
  DataFusion's tested algorithm without its execution layer, and maintain
  the copy (§11).
- **Implement:** write our own, when DataFusion has nothing equivalent
  (PromQL, sketches) or the operator is a few lines.

§9 lists which operator is which.

### 7.4 Classify operators by their state and how they split

Each operator is described by how its state grows with its input (memory)
and whether its input can be split across threads (parallelism). §8.1 and
§8.2 give the classification and place every operator in it.

### 7.5 Grouped state through `GroupsAccumulator`

Aggregates, summary builds and summary merges are DataFusion's
`GroupValues` plus one `GroupsAccumulator` per function, with no grouping
code of ours. SQL aggregates use DataFusion's accumulators; each sketch
family and each PromQL aggregate is our own (§8.3).

### 7.6 Sketches stay in memory between operators

DataFusion's `state` encodes states as bytes. Passing sketches between our
operators that way would encode and decode them at every step. Instead,
`push` takes our `Batch`, whose summary columns index into its in-memory
sketch list (§3), and our accumulators hand sketches to the next operator
in memory. Encoding happens only when a state is stored. DataFusion functions
receive the `Batch`'s `RecordBatch`.

### 7.7 Evaluation time enters at `open`

`Operator::open(ctx)` (§5) creates each run's state, and `ctx`
holds the evaluation time; operators receive no other run context. PromQL's
`@` (evaluate at another time) stays a runtime action: the runtime opens the
operator's inputs with a shifted `ctx`.

### 7.8 Later: results per window within one run

Not in the first version. Each ingestion run covers one window, and
`finish` emits its result. An `advance(t)` method, emitting the windows that
end at or before `t` without ending the run, would let one operator
instance span many windows. If added: `t` is a window end the runtime
already knows, late data is dropped, and operator state is not
checkpointed.

### 7.9 DataFusion's expression semantics win

Where our expression semantics differ from DataFusion's (§13), we adopt
DataFusion's and accept the changed query results. This includes PromQL
functions: they map onto the nearest DataFusion built-in, and become our own
UDF only when no built-in is close enough. The checks that fail a query
(`FiniteFloat64`, checked division, exact Int64→Float64) are dropped.

## 8. Design details

### 8.1 Classifying aggregate functions

The classic classification (Gray et al., data cube) is of **functions**:
what is the smallest partial result from which the exact answer can still
be computed?

| Class | Partial result | Examples |
| --- | --- | --- |
| **Distributive** | the function's own value: `F(X ∪ Y) = G(F(X), F(Y))` | sum, count, min, max; building a sketch |
| **Algebraic** | a fixed number of distributive values, then a final step | avg (sum and count, then sum / count), stddev |
| **Holistic** | none of fixed size | exact quantile, median, exact distinct count |

A sketch involves three functions, each classified on its own. For p99 via
KLL:

- **Building the sketch** is distributive: two sketches merge into one.
  CMS and HLL merge into exactly the sketch built from all the data; KLL
  into a different sketch with the same error guarantee.
- **Reading p99 from the sketch** is not an aggregate: it is a scalar
  function of one state.
- **Exact p99**, which the sketch approximates, is holistic.

This is why ASAP uses sketches: a holistic function's answer comes from a
distributive, fixed-size build, which can be updated per mini-batch at
ingestion time and merged across windows at query time.

### 8.2 Classifying operators

We analyse two properties of an operator in a query:

- **Memory:** how much state it keeps.
- **Split:** whether its work can be spread across threads. Version 1 runs
  single-threaded (§7.2), so this is analysis for later.

Three factors determine them:

1. **What one state grows with.** Set by the function or operator.
   - *Nothing (fixed size):* sum, avg, a KLL sketch.
   - *Distinct values:* exact distinct count.
   - *Rows:* exact quantile, sort, join.
2. **Whether groups share a state.** Set by the function.
   - *Per group:* each group has its own state (`sum by (job)`).
   - *Shared:* one state mixes all groups (Hydra).
   - *None:* the operator keeps nothing (`filter`).
3. **The query's groups.** Set by the query.
   - *How many groups* there are.
   - *Whether a column exists to route rows by:* a `by`, a join key, or a
     `PARTITION BY`.

The function classes of §8.1 explain factor 1: distributive and algebraic
functions, including sketch builds, have fixed-size state; holistic
functions have growing state.

The logic:

- **Memory** = number of states × size of one state. The number of states
  is the number of groups, 1 if shared, or 0 with no state. One state's
  size is fixed, or its group's distinct values or rows.
- **Split:**
  1. No state: **any split**.
  2. Fixed-size state: **any split + merge**. Each thread builds a small
     partial state, and the partials merge.
  3. Growing state, with a column to route by: **partitioned** by it.
  4. Growing state, no such column: **single thread**.

  Sort is the exception: its state grows with rows, but sorted runs from
  any split can be k-way merged.

  `GroupsAccumulator::state` and `merge_batch` (§8.3) already provide the
  merge that "any split + merge" needs.

Every operator, by factor and outcome:

| Operators | State grows with | State per | Memory | Split |
| --- | --- | --- | --- | --- |
| `filter`, `project`, `time_range`, `evaluate_at`, `union`, `series_labels`, `series_relabel`, `window_end_timestamp`, `evaluation`, scalar expressions | — | none | none | any |
| `aggregate` with distributive or algebraic functions, sketch builds, `summary_merge`, `current_series`, `limit` | nothing | group | groups × fixed size | any + merge |
| `all_groups_summary_build` (Hydra) | nothing | shared | fixed size | any + merge |
| `aggregate` with exact distinct count | distinct values | group | groups × distinct values | partitioned by group, else single thread |
| `aggregate` with exact quantile, joins, `set_operation`, `sql_window`, `range_window` / `series_window` | rows | group | rows | partitioned by group, join key, partition key or series, else single thread |
| `sort` | rows | group | rows | any, then k-way merge |

An aggregate takes the worst of its functions: one exact quantile puts the
whole operator in the "rows" row.

### 8.3 Grouped state through `GroupsAccumulator`

Aggregates, summary builds and summary merges all keep one state per
group. DataFusion splits this into two types:

- **`GroupValues`:** maps each row's key columns to a group id, a batch at
  a time: `[api, db, api]` → `[0, 1, 0]`.
- **`GroupsAccumulator`:** one object holding every group's state:
  - `update_batch(values, group_ids)`: adds a batch's values to their
    groups.
  - `evaluate`: one result per group, e.g. p99 per `job`.
  - `state`: each group's state as Arrow arrays, to store.
  - `merge_batch(states, group_ids)`: merges stored states into groups.

Each sketch family and each PromQL aggregate becomes our own
`GroupsAccumulator`; SQL aggregates use DataFusion's. An aggregate operator
is then `GroupValues` + accumulators, with no grouping code of ours.

`init / update / finalize` correspond to creating an accumulator,
`update_batch` and `evaluate`. ASAP also needs `merge_batch`, to combine
1-minute window states into longer windows. A `retract` (remove values) is
not needed: most sketches cannot remove values, so sliding windows merge
window states instead.

## 9. Per-operator mapping

The names below are the `Operator` constructors; their code is in
`crates/executor/src/operators/`, mostly in a file of the same name (e.g.
`union`, `scalar` and `absent` are in `source.rs`, `window` in
`aggregate/`, `evaluate_at` in `time_range.rs`).

| Operator | State (§8.2) | How | Detail |
| --- | --- | --- | --- |
| `project`, `filter`, scalar `Expression` | none | Wrap + implement | Expressions are lowered to DataFusion `PhysicalExpr`s; `filter` then applies Arrow's `filter` kernel. About half of our expression forms map directly, a third need care when lowering, and the rest are ours (§13). |
| `aggregate` | per group; growth from its functions | Wrap + implement | Grouping uses `GroupValues`, and SQL aggregates use DataFusion's accumulators. PromQL `avg`, `quantile`, compensated `sum` and the NaN rules of `min`/`max` differ from DataFusion's, so they are our own `GroupsAccumulator`s. |
| `sql_window` | per group, grows with rows | Wrap | SQL window functions use DataFusion's built-in implementations (`PartitionEvaluator`), whose semantics `sql_window` already follows. |
| `sort` (grouped, optional top-k) | per group, grows with rows | Wrap + copy | At `finish`, the buffered rows are concatenated and sorted by DataFusion's public `sort_batch`. Top-k is copied from DataFusion's `TopK`, which is private, so it keeps only k rows per group instead of every row. |
| `relational_join`, `semi_join` | per group, grows with rows | Wrap + copy | The build side uses DataFusion's public `JoinHashMap`. The probe and the output of each join kind (inner, outer, semi, anti, non-equality predicates) are copied from `HashJoinExec`. |
| `certified_semi_join`, `set_operation` | per group, grows with rows | Implement | Both are built on the copied join: the first adds a check that the right side is complete, the second adds a distinct, as DataFusion plans INTERSECT and EXCEPT. |
| `union`, `limit`, `scalar`, `source`, `absent`, `vector_to_scalar`, `bind_scalar_ref` | none (`limit`: per group, fixed size) | Implement | These are a few lines each on Arrow's `concat` and `slice`, so reuse saves nothing. |
| `per_group_summary_build`, `per_group_keyed_summary_build`, `summary_merge` | per group, fixed size | Wrap + implement | Grouping uses `GroupValues`; each sketch family is our own `GroupsAccumulator` (§8.3). |
| `all_groups_summary_build` | for all groups, fixed size | Implement | HydraCms keeps one state shared by all groups, which `GroupsAccumulator` cannot express. |
| `evaluation`, `keyed_evaluation` | none | Implement | They read a value (a quantile, a count) from a summary state, which is sketch-specific. |
| PromQL and time: `time_range`, `evaluate_at`, `scope_timestamp`, `current_series`, `series_window`, `range_window`, `window`, `*_binary`, `series_labels`, `series_relabel`, `series_sample`, `*histogram_quantile` | Varies (§8.2) | Implement | DataFusion has no PromQL or time-series operators. Their grouping and label matching can use `GroupValues` and Arrow's `RowConverter`. |

## 10. Risks

- **Semantics.** Adopting DataFusion's expression semantics (§7.9) changes
  query results, including PromQL edge cases (NaN, rounding, overflow) where
  we will then differ from Prometheus. Grouping, accumulators, sort and
  joins must not change results.
- **Reading a state twice.** `GroupsAccumulator::evaluate(EmitTo)` and
  `state(EmitTo)` reset the groups they emit. A sliding window reads the
  same window state several times, so our sketch accumulators also provide
  non-resetting reads, which our operators call directly.
- **One producer, several consumers.** DataFusion's streams and
  accumulators cannot be cloned, while a node of our plan can have several
  readers. Streams do not matter, since we use no `*Exec`. Our runtime gives
  each reader a cheap clone of each batch: the Arrow arrays and the sketch
  list are shared, not copied, and no reader modifies them.
- **Unstable DataFusion interfaces.** `GroupValues` and `JoinHashMap` are
  public but not a stable API, and can change between major versions. We
  pin DataFusion (§11) and adapt our code on each upgrade.
- **Buffering between operators.** The driver feeds a two-input operator
  all of input 0 before input 1 (§7.1). If both inputs come from the same
  upstream operator, e.g. one scan feeding both sides of a join, that
  operator's output for input 1 is held until input 0 ends: up to a whole
  side of the join. This is a property of the plan's shape, handled by the
  runtime, not by the operators.

## 11. Implementation details

- **DataFusion 54.2.0.** It is the newest release on our Arrow version (58),
  so no Arrow upgrade is needed. Releases and their Arrow versions: 54.x →
  58, 55.x → 59, 56 (release candidate, Nov 2026) → 60. Every API this doc
  uses is public in 54.2.0. Each later DataFusion upgrade may force an
  Arrow upgrade, which reaches every deployment that builds against
  `asap-executor`.
- **`GroupValues` comes from `datafusion-physical-plan`,** which also pulls
  in the execution layer's crates at compile time, unused. Copy
  `GroupValues` instead only if build time or version conflicts hurt.
- **Copied files** keep DataFusion's Apache-2.0 header, are listed in
  `THIRD_PARTY.md`, and record the DataFusion commit they came from.

## 12. Finding: operators in ingestion-time plans

Every ingestion-time plan in the corpus snapshots is a scan followed by a
summary chain: `filter` / `project`, then `time_range` (one window's rows),
`per_group_summary_build` (a sketch per group) and `window_end_timestamp`
(stamps each result with its window end). The planner does not restrict
which operators may run at ingestion time, so others can appear later.

## 13. Finding: expressions on DataFusion `PhysicalExpr`

We adopt DataFusion's semantics for every difference below (§7.9).

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
    comparison, `IN` and `CASE x WHEN`.
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

## 14. Implementation plan

Every PR keeps the executor working. "Tests unchanged" means the existing
executor tests and corpus snapshots pass without edits.

### Phase A: new interface, same internals

| # | PR | Done when |
| --- | --- | --- |
| A1 | Remove memory accounting, yielding and cancellation: `Limits`, budgets, `cooperative.rs`, their checks in operators, and their tests. `RunContext::new` loses its `Limits` argument. File issues in ASAPQuery-backend and sketch-bench for the API break. | Remaining tests unchanged. |
| A2 | Add `OperatorLogic`, `Operator::open` and `drive`; port `filter`. `drive` feeds inputs in index order: every batch of input 0, `finish(0)`, then input 1. Sources (`Scan`, `UnboundInput`) stay async streams. | Tests unchanged. |
| A3 | Port the stateless operators (projection, `time_range`, `scope_timestamp`, `bind_scalar_ref`, `unchecked`, `limit`, `series_labels`, `series_sample`). | Tests unchanged. |
| A4 | Port the grouping operators (aggregate, summary builds and merge, `range_function`, `series_window`, `vector_window`, `current_series`). | Tests unchanged, plus a new test: a KLL sketch built from many mini-batches equals one built from a single batch. |
| A5 | Port sort, joins and the other two-input operators (sort, joins, `aligned_binary`, `vector_binary`, `set_operation`, `sql_window`). The semi-join builds on input 1, so it buffers input 0 until Phase B. | Tests unchanged, plus a new test: a join whose two inputs read the same upstream operator completes, with input 1's batches waiting in the runtime's per-reader queue (§10). |
| A6 | Delete the old path: `PhysicalOperator::start` for operators and the per-operator `execute` functions. Test operators implement `OperatorLogic`. | Tests unchanged. |

### Phase B: DataFusion internals

| # | PR | Done when |
| --- | --- | --- |
| B1 | Add DataFusion pinned to exactly `=54.2.0` (§11), since `GroupValues` is not a stable API (§10); `GroupValues` replaces `group_table`. The PR states the added clean-build time and crates, and one before/after timing of a grouped aggregate. | Tests unchanged. |
| B2 | Built-in SQL aggregates use DataFusion's `GroupsAccumulator`s; our sketches implement the same trait (§8.3). | Tests unchanged. |
| B3 | Sort on DataFusion's `sort_batch`; top-k copied from DataFusion's `TopK`. | Tests unchanged. |
| B4 | Joins on `JoinHashMap`, probe copied from `HashJoinExec`. The build side moves to input 0, which ends the semi-join's buffering. | Tests unchanged. |
| B5 | Expressions lowered to `PhysicalExpr` with DataFusion's semantics (§7.9); PromQL functions on built-ins or our UDFs. | Tests updated only for the result changes listed under the PR's **Verification**, each traced to a §13 difference. |
| B6 | Re-measure per-operator unit costs on the new executor and update `operations_per_unit` in `asap-cost`. | Before/after timings reported; any planner snapshot changes listed with why. |

Efficiency gates no PR before B6: the main goal is less code to maintain.

