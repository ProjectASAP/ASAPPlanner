# Multi-level tumbling-window chains

**Status:** Accepted; implemented.

**Audience:** Planner designers and implementers.

A long range read on a short evaluation grid merges many narrow windows at every
step. This proposal lets the planner add wider tumbling-window chains (levels)
next to the narrowest one and cover each step's range with the widest whole
windows that fit, using the narrow windows only at the two edges. Stage 3
chooses the levels by cost, as it chooses every other candidate.

## 1. Problem, with a measured example

Workload: `quantile_over_time(0.99, data[1h])`, a range request with step 15 s
and span 1 h (241 steps), requested every 15 s, 10,000 series at 10 Hz
(100,000 rows/s), accuracy 0.05 relative.

Plan selected on main today (`asap_planner --run-until stage3 --format text`,
cost model 1):

```
n0:  C2 = G0[0] (IngestionTime, None)                        Scan
n1:  C77 = G8[0] (IngestionTime, AcrossEvaluations(Memory))  SummaryTumblingWindowChain DDSketch tumbling_window_width_ms=15000  θ=DDSketch(alpha=0.05)  <- n0
n2:  C103 = G10[0] (QueryTime, None)                         TimeRange 3600s                                                     θ=DDSketch(alpha=0.05)  <- n1
n3:  C47 = G5[7] (QueryTime, OneEvaluation(Memory))          SummaryMerge                                                        θ=DDSketch(alpha=0.05)  <- n2
n4:  C58 = G6[0] (QueryTime, None)                           SummaryEstimate Quantile(q=0.99)                                                            <- n3
n5:  C10 = G2[2] (QueryTime, None)                           Project                                                                                     <- n4
cost_per_second: 9.896383e4
    n1  SummaryTumblingWindowChain  IngestionTime  1.120917e3   build DDSketch in tumbling windows of 15000 ms for 10000 groups: 100000 rows x 210 ops, 1000 group visits, 666.7 states emitted; x 1.0000/s; keeps 8773440000 bytes
    n2  TimeRange                   QueryTime      4.241601e2   time range 3600s: 240 tumbling windows of states; x 16.0667/s; holds 1824 bytes for 26.400 ms
    n3  SummaryMerge                QueryTime      9.685584e4   merge 2400000 states into 10000; x 16.0667/s; holds 23120000 bytes for 6011.000 ms
```

Every step merges 240 windows per series, 2.4 million DDSketch merges. The
model charges 6.0 s per step; a run measured 4.0 s per step, about 16 minutes
per 241-step request, so every request hits a 60 s timeout. The merge is 98% of
the plan's cost.

What exists and why it does not help:

- **Chains.** Pass 2 (`add_chain_groups`, `stage1-pass2-reuse-candidates/src/rules/chains.rs`)
  adds one chain group `SummaryTumblingWindowChain[w](X)` per width in the gcd
  closure of `gcd(evaluation period, range, offsets)`. Here that set is
  `{gcd(15 s, 1 h)} = {15 s}`: one width. No wider width is ever proposed, and
  no chain reads another chain. (The operator was once named `PaneChain`; the
  serde alias is all that remains of the name.)
- **Window selection.** A `TimeRange(W)` over a chain selects the windows ending
  in `(T − W, T]` (cost: `selected_tumbling_windows` in
  `cost/src/node/usage/mod.rs`; the executor's `MaterializedViewRequest`).
  Over a wider chain that is wrong whenever `T` is not on the wider grid: the
  oldest selected window starts before `T − W`. So a wider chain cannot simply
  replace the 15 s chain.
- **Rollups.** The rollup rule merges states across keys (`SummaryMerge` with
  `group_by`), not across time.
- **Stage 3.** Cost model 1 already charges query-time work per step times
  steps: `x 16.0667/s` above is 241 steps per request every 15 s
  (`executions_per_second` → `evaluation_rate`,
  `cost/src/plan/execution_demand/mod.rs`). A plan that merges fewer windows
  per step is cheaper in proportion, so Stage 3 needs no change for cost
  model 1; only the selected-window count of the new range node (section 3) is
  new. Cost model 3's latency counts range work per step the same way.

## 2. Components

1. **Levels.** For a chain group of width `w` read by a range `W`, Pass 2 adds
   chain groups of wider widths `C` from a fixed ladder (1 min, 5 min, 15 min,
   1 h, 6 h, 1 d), kept when `w` divides `C`, every narrower level's width
   divides `C`, and `2·C ≤ W`; at most three (of more, the narrowest, the
   middle one and the widest), only over a chain whose width divides the
   state's evaluation grid, and only for the smallest θ that met the
   state's accuracy target (a larger θ's levels hold larger states at more
   cost). All levels are anchored at the epoch, like today's chains. A wider level need not divide the evaluation grid: only the
   narrowest level must (today's rule), because the edges are read from it.
2. **One build per level.** The group `SummaryTumblingWindowChain[C](X)` gets
   one candidate per θ: a new operator `SummaryMergeWindows[C]` over the next
   narrower level: when a window of width `C` ends, it merges the `C/c`
   narrower windows it contains (one merge per narrower state, no row work).
   Stage 2 places it only at ingestion time, materialized across
   evaluations. A cover reads a wider level only when it is stored at
   ingestion time, so its windows are on the epoch grid; that rules out a
   from-rows chain of the same width built at query time (whose windows end
   at the evaluation time).
3. **Cover.** A new node `TimeRangeCover(W)` over the levels (each under the
   same `TimeShift`s), widest first, selects at each evaluation time `T` the
   windows that tile `(T − W, T]` exactly: the widest whole windows inside the
   range, then, in each uncovered edge, the next level's whole windows, down to
   the narrowest level. The state group `SummaryAgg(TimeRange(W, X))` gets
   `SummaryMerge` over each of two covers as candidates: over the first
   wider level, {1 min, 15 s}, and over all of them, {15 min, 5 min, 1 min,
   15 s}.
4. **Cost.** The cover's selected windows per step are the mean, over the
   evaluation times of the reader it runs for (its context's grid), of the
   cover's window count (closed form below), not rounded; a `SummaryMerge`
   over it is charged per input state, as today. Stage 3 costs a cover, and
   its readers, at each context's grid, so its objective is the plan's
   cost. Each level's
   retention `h_v` is its longest reader's, as today.
5. **Executor.** Runs `SummaryMergeWindows` in an ingestion run (a range
   over the narrower windows, each stamped with the end of its wider window
   on the epoch grid, merged per key), and `TimeRangeCover` as one range per
   level that keeps the windows the cover takes from it, then their union. A
   query run reads a chain's stored windows whole: a wider level read off its
   grid reads its whole windows inside the range.

## 3. Algorithm

**Cover at time `T`.** Levels `c_1 > c_2 > … > c_L`, each dividing the next
wider, `c_L` dividing the evaluation grid, the range `W`, and the offsets.

```
cover(a, b, level i):              # windows tiling (a, b]
    if a >= b: return []
    if i == L: return windows of c_L ending in (a, b]   # a, b on the c_L grid
    lo = ceil(a / c_i) * c_i;  hi = floor(b / c_i) * c_i
    if lo >= hi: return cover(a, b, i + 1)
    return windows of c_i ending in (lo, hi]
         + cover(a, lo, i + 1) + cover(hi, b, i + 1)
TimeRangeCover(W) at T = cover(T − o − W, T − o, 1)    # o = summed shifts
```

Each window lies inside `(T − W, T]`, and the windows are disjoint and cover
the range, because each level's boundaries are boundaries of every narrower
level. This holds whether or not `W` is a multiple of `c_1`: a range that is not
just leaves longer edges for the narrower levels.

**Windows merged per step** (computed with the function above over one hour of
15 s steps, `W` = 1 h unless stated):

| Levels | Mean | Max | Min |
| --- | --- | --- | --- |
| 15 s (today) | 240 | 240 | 240 |
| 5 min, 15 s | 30.1 | 31 | 12 |
| 4 min, 15 s | 29.1 | 30 | 15 |
| 5 min, 1 min, 15 s | 18.05 | 19 | 12 |
| 15 min, 5 min, 1 min, 15 s | 12.02 | 13 | 4 |
| 5 min, 15 s with `W` = 57.5 min (not a multiple of 5 min) | 29.6 | 40 | 21 (flat: 230) |

Example: `T` = 100 h + 105 s, levels 5 min and 15 s. The range is
`(99 h + 105 s, 100 h + 105 s]`: 11 whole 5 min windows `(99 h + 300 s, 100 h]`,
13 windows of 15 s at the old edge `(99 h + 105 s, 99 h + 300 s]` and 7 at the
new edge `(100 h, 100 h + 105 s]`: 31 merges instead of 240.

**Mean selected windows (cost), closed form.** The levels from the widest
down to `i` cover the whole windows of `c_i` inside the range:
`U_i = max(0, ⌊T⌋_{c_i} − ⌈T − W⌉_{c_i})`, so level `i` takes
`(U_i − U_{i−1}) / c_i` windows. On a grid of period `p` and phase `φ`, the
residues of `T` modulo `c_i` are `φ mod g + j·g` (`g = gcd(p, c_i)`,
`j < c_i/g`), each equally often, so `E[U_i] = W − E[T mod c_i] −
E[(c_i − (T − W) mod c_i) mod c_i]` when `W ≥ 2·c_i` (the `max` cannot
bind); otherwise the `c_i/g` residues are summed. Example, 1 h over 15 min,
5 min, 1 min and 15 s: 12.02 windows every 15 s, 9.07 every 60 s.

**Predicted plan after** (per-step merge work scales with merges; the state
size of a 5 min DDSketch is larger, which the cost model's state bytes already
track):

```
n0:  Scan
n1:  SummaryTumblingWindowChain DDSketch tumbling_window_width_ms=15000   (IngestionTime, AcrossEvaluations(Memory))  <- n0
n2:  SummaryMergeWindows tumbling_window_width_ms=60000                   (IngestionTime, AcrossEvaluations(Memory))  <- n1
n3:  SummaryMergeWindows tumbling_window_width_ms=300000                  (IngestionTime, AcrossEvaluations(Memory))  <- n2
n4:  SummaryMergeWindows tumbling_window_width_ms=900000                  (IngestionTime, AcrossEvaluations(Memory))  <- n3
n5:  TimeRangeCover 3600s                                                 (QueryTime, None)  <- n4, n3, n2, n1
n6:  SummaryMerge                                                         (QueryTime, OneEvaluation(Memory))  <- n5
n7:  SummaryEstimate Quantile(q=0.99)  <- n6
n8:  Project  <- n7
```

| Levels | Merges / step | Model ms / step | Measured-scaled s / step | s / request (241 steps) |
| --- | --- | --- | --- | --- |
| 15 s (today) | 240 | 6,011 | 4.0 | 964 (measured ~16 min) |
| 5 min, 15 s | 30.1 | ~750 | 0.50 | ~121 |
| 5 min, 1 min, 15 s | 18.1 | ~450 | 0.30 | ~73 |
| 15 min, 5 min, 1 min, 15 s | 12.0 | ~300 | 0.20 | ~48 |

"Measured-scaled" is 4.0 s × merges / 240. Building the wider levels from the
narrower one costs, per series per 15 min, 60 merges (1 min from 15 s), 15
(5 min from 1 min) and 3 (15 min from 5 min): about 870 merges per second for
10,000 series, roughly 2 ms of CPU per second, against 100,000 row updates per
second for the 15 s chain. Building them from the rows instead would add a full row
update per level. Even four levels leave a 241-step request near 48 s on one
core; staying under a 60 s timeout with margin also needs the steps run in
parallel, which is outside this proposal.

**Memory.** The 15 s chain keeps 8.77 GB (480 windows × 10,000 series ×
1,828 B). Its retention is unchanged: the old edge still reads 15 s windows
near `T − W`. Each wider level keeps `retention / C` windows: 120, 24 and 8
windows per series for 1 min, 5 min and 15 min, about 152 states per series
against 480, so +32% states; DDSketch states of wider windows are larger
(more buckets), bounded by the sketch's bucket limit, so the bytes rise by
+32% to roughly +60%. Stage 3 charges this through each level's state bytes
and retention, so it can decline a level whose memory outweighs its merges.

**Exactness.** A level is offered only for summary types whose capability row
merges (`merge_inputs`), the same condition as for chains today; the cover
merges the same kind of states, only fewer and wider.

| Family | Two-level merge vs. flat merge |
| --- | --- |
| DDSketch (same α), Count-Min, Count Sketch, HLL, KMV, exact sum / count / min / max | Identical state: merging is order-free and equals the direct build. |
| Counter increase / rate states | Identical: windows are disjoint and contiguous, so the per-window increases add the same way. |
| KLL | Not bit-identical (compaction depends on merge order), but keeps its rank bound: the bound holds for any merge tree. |
| Heap top-k, UnivMon entropy | Not offered, as for rollups today (no merged guarantee). |

**Complexity (planner).** For each chain group: at most 3 levels and two
covers, each for the smallest θ each reader of the state accepts (one θ when
the readers share a target). Planning time over the 56
snapshot workloads is in the PR body (before and after the caps).

## 4. Code interface

```diff
 // crates/types/src/ir/operator/asap.rs
 pub enum ASAPOp<C> {
     SummaryTumblingWindowChain { child: C, summary_type: FieldDataType, input: SummaryUpdate,
         reduction: Reduction, filter: Option<Predicate<C>>, tumbling_window_width_ms: u64 },
+    /// Wider windows built from a narrower chain `child`: window `(k·w, (k+1)·w]`
+    /// is the merge of `child`'s windows inside it, per key. A candidate of the
+    /// group `SummaryTumblingWindowChain[w](X)`; Stage 2 places it at ingestion time.
+    SummaryMergeWindows { child: C, tumbling_window_width_ms: u64 },
 }

 // crates/types/src/ir/operator/non_asap.rs
 pub enum NonASAPOp<C> {
     TimeShift { shift: TimeShift, child: C },
+    /// The windows of `levels` (chains, widest first, each width a multiple of
+    /// the next) that tile `(T − range, T]`: widest whole windows, narrower edges.
+    TimeRangeCover { range: Duration, levels: Vec<C> },
 }

 // crates/types/src/ir/operator/node.rs
 impl<C> Operator<C> {
+    /// Whether it reads input `input` off its own evaluation grid on purpose: a
+    /// `SummaryMergeWindows`'s narrower chain, a cover's wider levels. Pass 2's grid
+    /// check and Stage 3's delivery check exempt exactly these; Stage 3 accepts
+    /// such an input only when stored at ingestion time (epoch-anchored).
+    pub fn reads_input_off_the_grid(&self, input: usize) -> bool;
 }

 // crates/types/src/ir/operator/kind.rs
 pub enum OperatorKind {
+    TimeRangeCover,
+    SummaryMergeWindows,
 }

 // crates/types/src/stages/logical/candidate/mod.rs
 pub enum OriginOfCandidateForMemoGroup {
     TumblingWindows,
+    /// A wider chain level, a cover over levels, or a merge over a cover.
+    MultiLevelWindows,
 }

 // crates/types/src/ir/operator/tumbling_window_chain.rs
+/// One level of a cover: the cover's range, its levels' widths and this level's index.
+pub struct CoverLevel { pub range_ms: u64, pub level_widths_ms: Vec<u64>, pub level: usize }
+/// An interval off the narrowest level's grid: no exact cover.
+pub struct CoverOffTheGrid { pub start_ms: i64, pub end_ms: i64, pub narrowest_ms: u64 }
+/// The windows tiling `(start_ms, end_ms]` (widest first): `(level, window_end_ms)`, oldest first.
+pub fn cover_windows(start_ms: i64, end_ms: i64, level_widths_ms: &[u64]) -> Result<Vec<(usize, i64)>, CoverOffTheGrid>;
+/// Mean windows taken per level at the evaluation times of `grid` (closed form).
+pub fn mean_cover_windows_per_level(range_ms: u64, grid: EvaluationGrid, level_widths_ms: &[u64]) -> Vec<f64>;

 // crates/cost/src/node/usage/mod.rs
-pub fn node_resource_usage_and_cost<'a>(node: &'a PhysicalASAPDAGNode, producers: &[ProducerOutput<'a>], rate: (u64, f64), window_ms: Option<u64>, data: &DataWorkload, settings: &'a CostInputs) -> Result<NodeResourceUsageAndCost<'a>, AnalyticalCostError>;
+/// `evaluation_grid`: the grid the node runs at (its readers'); a cover is charged its mean windows there.
+pub fn node_resource_usage_and_cost<'a>(node: &'a PhysicalASAPDAGNode, producers: &[ProducerOutput<'a>], rate: (u64, f64), window_ms: Option<u64>, evaluation_grid: Option<EvaluationGrid>, data: &DataWorkload, settings: &'a CostInputs) -> Result<NodeResourceUsageAndCost<'a>, AnalyticalCostError>;
 pub struct ProducerOutput<'a> {
-    pub tumbling_windows: u64,
+    /// A cover's is its mean windows per evaluation, not a whole number.
+    pub tumbling_windows: f64,
 }

 // crates/executor/src/operators (time_range.rs)
 impl Operator {
+    /// Keep only the windows a cover takes from one of its levels.
+    pub fn with_cover_level(self, cover_level: CoverLevel) -> Result<Self, Error>;
+    /// Tumbling-window ends anchored at the epoch, not at the range's end.
+    pub fn with_tumbling_window_ends_from_epoch(self, width_ms: i64) -> Result<Self, Error>;
 }
```

Pass 2: `add_chain_groups` adds the wider levels and one cover per chain
(`add_wider_levels`, `rules/chains.rs`), for the θs each reader of the state
accepts at least size. Cost: a cover is charged its mean windows on its
readers' grid; `SummaryMergeWindows` one merge per narrower state plus the
emit charge per wider state, at ingestion time. Stage 2 places
`SummaryMergeWindows` only at ingestion time. Stage 3 costs a cover and its
readers at each context's grid and accepts a cover's wider level only when
stored at ingestion time. The executor runs both operators; a slot read only
by covers fetches the union of the windows they take from it. Retention: each
level keeps its readers' whole range back from the newest evaluation (a
store keeps a contiguous span; the cover's oldest edge is `W` back).

**Deployments without the operators.** A runtime that cannot run them
lists both in `excluded_candidates`: `[{"operator": "SummaryMergeWindows"},
{"operator": "TimeRangeCover"}]`. Excluding `SummaryMergeWindows` alone is
not enough: a cover may still read a coarser chain built from the rows
(for a reader on a coarser grid).

## 5. Decisions

1. **Wider levels are extra chain groups, not a new kind of chain.** The memo
   already keys a chain by its width; a wider level is the same logical
   expression with a wider width, so its retention and its cost are Stage 3's
   as for any chain. Rejected: a single "hierarchical chain" operator with a
   list of widths, which would hide each level from Stage 3.
2. **A new range node instead of changing `TimeRange`.** `TimeRange` over a
   chain keeps its meaning (windows ending in the range); the cover's rule is
   in one place. Rejected: giving `TimeRange` an alignment field, which would
   change every existing chain read.
3. **Fixed ladder of widths.** Widths that match how ranges are written
   (1 min … 1 d) and divide each other, so any prefix nests. The best single
   width for range `W` and narrowest `w` is near `sqrt(W·w)` (4 min for 1 h
   over 15 s: 29.1 vs 30.05 at 5 min), a 3% difference that does not pay for
   computed, non-nesting widths.
4. **`2·C ≤ W`.** A level of width `C` contributes whole windows to every step
   only when `W ≥ 2·C`; with `W < 2·C` some steps take none and the narrower
   edges cost as much as today.
5. **Only the narrowest level must divide the evaluation grid.** The edges are
   always read from it, so wider levels may be off the grid (5 min under a
   15 s grid). Pass 2's grid check and Stage 3's delivery check exempt a
   cover's wider levels and a `SummaryMergeWindows`'s input.
6. **Two covers: the shortest and the longest prefix.** Pass 2 offers a
   cover over the first wider level (least memory) and one over every
   admitted level (fewest merges per step), not one per prefix (k) or per
   subset (2^k): each further cover multiplies the selection's size per
   context and θ, and the middle prefix of three rarely wins over both.
   Offering only the longest made one counter workload 3% dearer (its
   plan read the 1 min level alone).
6b. **Level θs per reader.** Levels and the cover are built for, per
   reader of the state, the smallest θ (by state size) whose edge meets
   that reader's accuracy, read from Pass 2's edges before the chain rule.
   A larger θ costs more for an accuracy a smaller one meets; taking only
   the smallest surviving θ instead left a stricter reader without a
   cover (+53% on one state read at 0.05 and at 0.005).
7. **A wider level is built only from the next narrower level.** A from-rows
   build adds one row update per level (100,000 per second here) against
   about 870 merges per second; it is not offered.
8. **No `TimeShift` in v1.** A range under an offset keeps its single 15 s
   chain; a cover would need the shifts above each level.

## 6. Cost model notes

Cost model 1 charges the cover's merge per step times steps (241 steps every
15 s: 16.0667 per second). It sums CPU core-seconds; neither it nor a
query's predicted `query_ms` (one evaluation) models running a request's
steps on several query worker threads, so the predicted request time is
steps × per-step work on one core: about 48 s at 10,000 series with four
levels.

## 7. Test plan (failing tests first)

Each test is written before the code and fails on main for the reason given.

| # | Test (crate) | Checks | Fails on main because |
| --- | --- | --- | --- |
| T1 | `cover_windows_tile_the_range` (types) | For levels {5 min, 15 s}, {5 min, 1 min, 15 s} and {15 min, 5 min, 1 min, 15 s}, every `T` on the 15 s grid over one day, `W` ∈ {1 h, 57.5 min, 30 min}: the windows are disjoint, inside `(T − W, T]`, and their union is the range. | `cover_windows` does not exist. |
| T2 | `cover_counts_match_the_design_table` (types) | `T` = 100 h + 105 s, {5 min, 15 s}: 13 + 11 + 7 = 31 windows; mean windows per 15 s step 30.05, 18.05 and 12.02 for the three level sets at `W` = 1 h. | As T1. |
| T2b | `closed_form_means_equal_counted_means`, `an_interval_off_the_narrowest_grid_is_refused` (types) | The closed-form means equal counts over every evaluation time, for ranges below and above twice the widest width, several periods and phases; an interval off the narrowest grid is an error. | Added after review. |
| T3 | `a_long_range_gets_wider_levels` (stage1-pass2) | `quantile_over_time(0.99, data[1h])`, step 15 s: chain groups at 15 s, 1 min, 5 min, 15 min (not 1 h: `2·C ≤ W`); each wider group's only candidates are `SummaryMergeWindows` over the next narrower group; the state group merges over covers over (1 min, 15 s) and over all four levels. | Pass 2 adds only the 15 s chain. |
| T4 | `a_short_range_gets_no_wider_level` (stage1-pass2) | `data[1m]` step 15 s: only the 15 s chain. | Passes on main; guards against regressions. |
| T5 | `wider_levels_need_a_merging_summary` (stage1-pass2) | An `irate` state (no merge) over an hour gets no chain and no level. | Passes on main; guards the capability rule. |
| T6 | `cover_merge_is_charged_its_mean_windows` (integration-tests) | The section 1 plan's merge is charged 120,000 states into 10,000 per step, × 16.0667/s; each `SummaryMergeWindows` is charged the states it merges per second at ingestion time. | Neither node is costed. |
| T7 | `stage3_picks_levels_for_a_long_range` (integration-tests) | The section 1 workload: Stage 3 selects wider levels and a cover; cost below main's 9.896383e4; at most 31 windows per series per step. | Pass 2 offers no levels. |
| T8 | `levels_cost_memory` (integration-tests) | Every level and the 15 s chain keep 7,200,000 ms; the 15 s chain keeps 8,773,440,000 bytes, as on main. | As T7. |
| T9 | `merge_windows_equals_direct_build` (executor) | Each 5 min window `SummaryMergeWindows` merges from 15 s windows holds what the 5 min chain built from the rows holds (exact sums per job, stamped with the window's end). | Operator not in the executor. |
| T10 | `cover_answers_equal_flat_answers`, `cover_maxima_equal_direct_maxima`, `cover_quantiles_equal_flat_quantiles`, `cover_kll_quantiles_keep_their_rank_bound`, `nested_ddsketch_levels_collapse_exactly` (executor) | `(T − 1h, T]` every 15 s over 5 minutes, on and off the 5 min grid: sums and maxima equal those computed over the raw samples; DDSketch p99 through two and through three levels equals the 15 s chain's bit for bit; KLL p99 within 2% rank of the true p99. | As T9. |
| T10b | `a_cover_off_its_grid_is_refused` (executor) | A query run at T off the 15 s grid is refused, not answered from part of the range. | Added after review. |
| T11 | `installable_plan_round_trips_levels` (integration-tests) | The plan file with both operators writes back the same text and compiles on the Planner's executor. | Operators do not exist. |
| T11b | `excluding_both_operators_plans_single_chains`, `a_cover_reads_wider_levels_only_from_ingestion_time` (integration-tests) | A deployment excluding both operators gets no level and no cover; a cover's wider levels are stored at ingestion time (never a chain built at query time). | Added after review. |
| T12 | Plan snapshots (devtools) | Corpus and sketch plan snapshots: only plans with a range read `W ≥ 2 min` change; each changed plan is listed in the PR with old → new cost. | Expected diff, reviewed, not a failing test. |

T10 runs exact sums (against sums computed directly over the raw samples)
and DDSketch p99s (cover against the 15 s chain, bit for bit), at every 15 s
step over 5 minutes, on and off the 5 min grid.

T12 result: 9 snapshots change, every one a range of at least 2 min read
through wider levels; each plan's cost falls (old → new, per second):

| Snapshot | Query | Old | New |
| --- | --- | --- | --- |
| o11y_bench q19 | `sum(increase(…[1h])) / sum(increase(…[1h]))` | 56.06 | 19.94 |
| o11y_bench q25 | `sum(rate(…[1h])) / sum(rate(…[1h]))` | 56.06 | 19.94 |
| o11y_bench q26 | `sum(rate(process_cpu_seconds_total[1h]))` | 27.61 | 9.45 |
| chains-10m-5m-3m-10s-mvp | quantiles over 10, 5 and 3 min | 5.109e-7 | 4.714e-7 |
| quantile-grouped-mvp | `avg by (region) (quantile_over_time(0.9, data[5m]))` | 1.903e-6 | 1.482e-6 |
| quantile-single-mvp | `quantile_over_time(0.99, data[5m])` | 1.823e-6 | 1.401e-6 |
| random-mixed-seed-862-long-w01-q3-mvp | `avg by (region) (quantile_over_time(0.95, data[300s]))` | 9.171e-5 | 2.508e-5 |
| random-mixed-seed-862-w02-q2-q7-mvp | a 100 s quantile and a 5 min top-k increase | 3.238e-5 | 3.203e-5 |
| topk-rate-guarantees-ignored-mvp | `topk(5, sum by (job) (rate(data_total[5m])))` | 3.619e-7 | 2.935e-7 |

