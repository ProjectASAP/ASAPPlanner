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

How to read this doc: §2 defines terms. §3 shows the code today, §4 the end
state, and §5 the decisions that take us from one to the other. §6 is the
path. §7 onwards is detail to read once the overview is clear.

## 2. Glossary

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
  collects its output, by calling the operator's methods (§4).
- **Query time / ingestion time:** a node runs either when a query is
  evaluated, or as data arrives, ahead of queries.
- **Window:** a fixed time interval, e.g. each 10-minute interval aligned to
  the clock. At ingestion time, each run of a plan covers one window.
- **Mini-batch:** one of the batches in which an operator's input arrives.
- **Batch mode / streaming mode:** in batch mode an operator's whole input
  already exists (e.g. read from storage for a query). In streaming mode it
  arrives over time as mini-batches (e.g. a window's data during
  ingestion). Either way the operator should process each mini-batch as it
  comes, without first collecting all of them (§5.1).
- **Aggregate:** an operator that outputs one final value per group, e.g.
  `sum by (job)`.
- **Summary:** a per-group state output as a column, to be merged or read
  later: a sketch (KLL, DDSketch, HLL, CMS) or an exact state (a running sum
  or count).
- **Accumulator:** the object that holds a group's state (e.g. its sketch)
  and is updated with values.
- **DataFusion component:** a public DataFusion building block that works
  on its own, given a `RecordBatch` or arrays, e.g. `PhysicalExpr`
  (evaluates an expression) or `GroupsAccumulator` (§5.4).
- **DataFusion execution layer:** what DataFusion's own operators run on:
  the `ExecutionPlan` trait, `TaskContext` and `RuntimeEnv` (memory pool,
  spill files), async streams and tokio. Each DataFusion operator, named
  `*Exec` (`SortExec`, `HashJoinExec`, `AggregateExec`), is built on it.

## 3. Today

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
  components        PhysicalExpr, GroupValues, GroupsAccumulator,
                    PartitionEvaluator, JoinHashMap, built-in functions and aggregates
  Arrow kernels     filter, take, sort, concat, RowConverter
```

DataFusion's operators (`*Exec`) are tied to its execution layer, just as
ours are tied to our runtime. Its components are not: they can be called
from any code.

## 4. End state

```text
runtime (ours): runs the plan, connects operators, reads storage
  │
  └─ driver (ours): open → push … push → finish
       │
       └─ operator: only the computation
            ├─ wrap:      calls DataFusion components (expressions, grouping, SQL aggregates)
            ├─ copy:      DataFusion code ported into our crate (sort, hash-join probe)
            └─ implement: our own code (PromQL, sketches, trivial operators)
```

The operator interface has two parts: an immutable definition, kept in the
compiled plan, and the per-run state it opens.

```rust
trait OperatorDefinition {
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

## 5. Decisions

### 5.1 Separate computation from running

Operators become plain computation (`OperatorLogic`, §4). The runtime and
its drivers own everything else. This is what makes the rest possible:
once an operator is only computation, its computation can come from
DataFusion.

One interface serves batch and streaming mode. The driver calls `push` for
each mini-batch, as soon as it is available, then `finish` when an input
has ended: at the end of a query's input, or at the end of an ingestion
window. An operator that can update its state per mini-batch must do so,
and not keep the rows: a 10-minute KLL sketch is updated with each
mini-batch, never built from 10 minutes of buffered rows. Some operators
cannot avoid keeping rows until `finish`: sort, a join's build side, set
operations. (`compute(batch)` and `init / update / finalize`, an earlier
two-interface idea, are a `push` returning one batch and an accumulator,
§5.4.)

The driver orders the inputs of a two-input operator: a join receives every
build-side batch and `finish` for that input before its first probe-side
batch.

### 5.2 Remove memory accounting, yielding and cancellation

From operators and from the runtime, to keep both simple. The runtime stays
async only because storage reads are async; operator calls are synchronous.
Operators run one at a time; parallel execution is out of scope for the
first version.

### 5.3 Use DataFusion's components, never its execution layer

Running a DataFusion `*Exec` from our runtime would mean building a small
DataFusion plan around our data:

```text
our batches ──▶ StreamLeaf (an ExecutionPlan we write) ──▶ SortExec ──▶ output stream
```

That brings back the execution layer: async pulling, tokio, a memory pool,
spill files. So every operator of ours is built in one of three ways:

- **Wrap:** call DataFusion components. No measurable overhead: it is a
  direct call on the `RecordBatch` our `Batch` already holds.
- **Copy:** port DataFusion code into our crate, when the logic exists only
  inside a `*Exec` (the hash-join probe) or is private (`TopK`). We keep
  DataFusion's tested algorithm without its execution layer, and maintain
  the copy (§9).
- **Implement:** write our own, when DataFusion has nothing equivalent
  (PromQL, sketches) or the operator is a few lines.

§7 lists which operator is which.

### 5.4 Grouped state through `GroupsAccumulator`

Aggregates, summary builds and summary merges all keep one state per
group. DataFusion splits this into two components:

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

### 5.5 Sketches stay in memory between operators

DataFusion's `state` encodes states as bytes. Passing sketches between our
operators that way would encode and decode them at every step. Instead,
`push` takes our `Batch`, whose summary columns index into its in-memory
sketch list (§2), and our accumulators hand sketches to the next operator
in memory. Encoding happens only when a state is stored. Wrapped DataFusion
components receive the `Batch`'s `RecordBatch`.

### 5.6 Evaluation time enters at `open`

`OperatorDefinition::open(ctx)` (§4) creates each run's state, and `ctx`
holds the evaluation time; operators receive no other run context. PromQL's
`@` (evaluate at another time) stays a runtime action: the runtime opens the
operator's inputs with a shifted `ctx`.

### 5.7 Later: results per window within one run

Not in the first version. Each ingestion run covers one window, and
`finish` emits its result. An `advance(t)` method, emitting the windows that
end at or before `t` without ending the run, would let one operator
instance span many windows. If added: `t` is a window end the runtime
already knows, late data is dropped, and operator state is not
checkpointed.

## 6. Path

Each step keeps the executor working, and its existing tests must pass
unchanged.

1. **Add the dependency:** DataFusion 54.2.0 (§9).
2. **Split computation from running:** add `OperatorDefinition` /
   `OperatorLogic` and a driver that runs them inside today's runtime.
   Remove memory accounting, yielding and cancellation, with the tests that
   cover them. Operators can then move one at a time.
3. **Stateless operators:** `project` and `filter` on `PhysicalExpr` (§11).
4. **Grouped operators:** aggregates, summary builds and merges on
   `GroupValues` + `GroupsAccumulator`.
5. **Sort and joins:** copied DataFusion code.
6. **PromQL operators:** our code, moved to `OperatorLogic`.

## 7. Per-operator mapping

The names below are the `Operator` constructors; their code is in
`crates/executor/src/operators/`, mostly in a file of the same name (e.g.
`union`, `scalar` and `absent` are in `source.rs`, `window` in
`aggregate/`, `evaluate_at` in `time_range.rs`).

| Operator | How | Detail |
| --- | --- | --- |
| `project`, `filter`, scalar `Expression` | Wrap + implement | Expressions are lowered to DataFusion `PhysicalExpr`s; `filter` then applies Arrow's `filter` kernel. About half of our expression forms map directly, a third need care when lowering, and the rest are ours (§11). |
| `aggregate` | Wrap + implement | Grouping uses `GroupValues`, and SQL aggregates use DataFusion's accumulators. PromQL `avg`, `quantile`, compensated `sum` and the NaN rules of `min`/`max` differ from DataFusion's, so they are our own `GroupsAccumulator`s. |
| `sql_window` | Wrap | SQL window functions use DataFusion's built-in implementations (`PartitionEvaluator`), whose semantics `sql_window` already follows. |
| `sort` (grouped, optional top-k) | Copy | Sorting uses Arrow's `lexsort_to_indices` and `take`. Top-k is copied from DataFusion's `TopK`, which is private. |
| `relational_join`, `semi_join` | Wrap + copy | The build side uses DataFusion's public `JoinHashMap`. The probe and the output of each join kind (inner, outer, semi, anti, non-equality predicates) are copied from `HashJoinExec`. |
| `certified_semi_join`, `set_operation` | Implement | Both are built on the copied join: the first adds a check that the right side is complete, the second adds a distinct, as DataFusion plans INTERSECT and EXCEPT. |
| `union`, `limit`, `scalar`, `source`, `absent`, `vector_to_scalar`, `bind_scalar_ref` | Implement | These are a few lines each on Arrow's `concat` and `slice`, so reuse saves nothing. |
| `per_group_summary_build`, `per_group_keyed_summary_build`, `summary_merge` | Wrap + implement | Grouping uses `GroupValues`; each sketch family is our own `GroupsAccumulator` (§5.4). |
| `all_groups_summary_build` | Implement | HydraCms keeps one state shared by all groups, which `GroupsAccumulator` cannot express. |
| `evaluation`, `keyed_evaluation` | Implement | They read a value (a quantile, a count) from a summary state, which is sketch-specific. |
| PromQL and time: `time_range`, `evaluate_at`, `scope_timestamp`, `current_series`, `series_window`, `range_window`, `window`, `*_binary`, `series_labels`, `series_relabel`, `series_sample`, `*histogram_quantile` | Implement | DataFusion has no PromQL or time-series operators. Their grouping and label matching can use `GroupValues` and Arrow's `RowConverter`. |

## 8. Risks

- **Semantics.** PromQL differs from DataFusion (NaN ordering, compensated
  sums, overflow as an error; §11). Wrapped and copied operators must pass
  our existing tests unchanged.
- **Reading a state twice.** `GroupsAccumulator::evaluate(EmitTo)` and
  `state(EmitTo)` reset the groups they emit. A sliding window reads the
  same window state several times, so our sketch accumulators also provide
  non-resetting reads, which our operators call directly.
- **One producer, several consumers.** DataFusion's streams and
  accumulators cannot be cloned, while a node of our plan can have several
  readers. Streams do not matter, since we use no `*Exec`. Our runtime gives
  each reader a cheap clone of each batch: the Arrow arrays and the sketch
  list are shared, not copied, and no reader modifies them.

## 9. Implementation details

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

## 10. Finding: operators in ingestion-time plans

Every ingestion-time plan in the corpus snapshots is a scan followed by a
summary chain: `filter` / `project`, then `time_range` (one window's rows),
`per_group_summary_build` (a sketch per group) and `window_end_timestamp`
(stamps each result with its window end). The planner does not restrict
which operators may run at ingestion time, so others can appear later.

## 11. Finding: expressions on DataFusion `PhysicalExpr`

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

## 12. Open questions

- **Why remove memory accounting and cancellation (§5.2)?** The reason
  should be stated: what bounds a runaway query or ends one whose client
  disconnected, and whether a long synchronous `push` on an async thread is
  acceptable.
- **How is "less code" measured?** E.g. lines removed against lines copied
  in, and the size of the DataFusion dependency.
- **How are semantic differences checked** beyond existing tests, e.g. by
  running our evaluator and DataFusion's on the same inputs for the
  expressions in §11 that differ?
- **Benchmark** before and after, for the secondary goal.
