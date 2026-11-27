# Wider tumbling windows per reader

**Status:** Design proposal. Not implemented. Builds on
[Multi-level tumbling-window chains](multi-level-window-chains.md).

**Audience:** Planner designers and implementers.

A state group read on several grids gets one chain width: the gcd of every
reader's grid, range and offsets. A reader on a coarser grid then merges as
many narrow windows as the finest reader does. This proposal lets each reader
read the widest windows its own grid allows, as a level built from the
narrow chain, and costs a cover by the grid of the reader it runs for.

## 1. Problem, with planner output

Workload: 10,000 series at 10 Hz, raw data not retained, two readers of one
expression `quantile_over_time(0.99, data[1m])`:

- q1, an instant query every 10 s;
- q2, a dashboard range request every 60 s, step 60 s, span 1 h (61 steps).

Plan before multi-level chains were merged (`asap_planner --run-until stage3 --format
text --deployment-capabilities {"raw_data_retained": false}`):

```
selection MILP: 280 variables, 387 constraints, objective 1.017759e3/s
n1:  SummaryTumblingWindowChain Kll tumbling_window_width_ms=10000  θ=Kll(k=2863)  (IngestionTime, AcrossEvaluations)
n2:  TimeRange 60s        <- n1   (q2's context)
n3:  SummaryMerge         <- n2
n6:  TimeRange 60s        <- n1   (q1's context)
n7:  SummaryMerge         <- n6
    n1  SummaryTumblingWindowChain  5.336100e2   build Kll in tumbling windows of 10000 ms ...
    n3  SummaryMerge                3.679765e2   merge 60000 states into 10000; x 1.0167/s
    n7  SummaryMerge                3.619441e1   merge 60000 states into 10000; x 0.1000/s
```

q2 merges 6 windows per series per step although each of its steps covers
exactly one minute on the minute grid: one 1 min window would do. Its merge
is 36% of the plan.

A second example, a 5 min range, `quantile_over_time(0.99, data[5m])` with the
same two readers:

```
objective 1.365800e3/s
n2:   SummaryMergeWindows tumbling_window_width_ms=60000  <- n1 (10 s chain)
n3:   TimeRangeCover 300s  <- n2, n1                       (q2's context)
    n3   TimeRangeCover   time range cover 300s: 4.17 + 5.00 tumbling windows of states per level; x 1.0167/s
    n4   SummaryMerge     2.405620e2   merge 90000 states into 10000; x 1.0167/s
```

Here q2 already reads a 1 min level, but the cost model averages the cover's
windows over every 10 s step (4.17 + 5 = 9.17 windows). On q2's 60 s grid the
cover takes exactly 5 windows of 1 min: the model over-charges q2's merge by
83%, so Stage 3 can miss plans that would pay.

### What exists and why it does not help

- **Widths.** Pass 2 adds a chain group per width in the gcd closure of
  `gcd(g_q, W, o)` over the readers: here 10 s (q1) and 60 s (q2) both exist
  as chain groups.
- **Group grid.** The edge from a chain into a state group is pruned when the
  chain's width does not divide the state group's evaluation grid, the gcd of
  all its readers' grids (`chain_off_the_evaluation_grid`, Pass 2): 60 s does
  not divide 10 s, so the 60 s chain serves nobody, though q2 alone could read
  it.
- **Stage 3 contexts.** Stage 3 runs each reader in its own evaluation
  context (q1's and q2's nodes above are separate) and already checks per
  context that a stored chain delivers on that context's grid
  (`delivers_on`). The group-level prune in Pass 2 is stricter than it needs
  to be.
- **A coarser single width for every reader** is never exact: the current
  width is the coarsest that divides every reader's grid, range and offsets.
  A coarser width can only serve the readers on its grid.

## 2. Components

1. **Reader levels.** For a chain group of width `w` serving state group `S`,
   each reader `q` of `S` with its own width `w_q = gcd(g_q, W, o_q)`, a
   multiple of `w` and wider than it, gets the level `w_q`: the chain group
   `SummaryTumblingWindowChain[w_q](X)` (already in the closure) gains a
   `SummaryMergeWindows[w_q]` candidate built from the widest level below it
   that divides it (the base chain, or a ladder level). Unlike ladder levels,
   a reader level needs only `w_q ≤ W` (one whole window per step).
2. **Per-reader grid check.** Pass 2 keeps the edge from a chain into the
   `TimeRange` (or cover) path of `S` when the chain's width divides the grid
   of at least one query reaching `S`, instead of the gcd of all of them.
   Stage 3's per-context delivery check then selects it only in contexts on
   its grid. Example: the 60 s chain serves q2's context, never q1's.
3. **Covers with reader levels.** Reader levels join the nested prefixes:
   levels are kept as divisibility chains from the base width; each maximal
   chain gives its nested prefixes (a reader level that nests with no ladder
   level forms a chain of its own: 10 s → 60 s; 30 s → 90 s next to 30 s →
   60 s).
4. **Cover cost by reader grid.** Done with the multi-level chains: a cover
   is costed on the grid of the context it runs in (closed form), and Stage 3
   costs each context's cover at its grid. The second example's 9.17 → 5
   windows is already charged on main.

## 3. Algorithm

**Levels of one chain group** (base width `w`, state range `W`, readers `q`):

```
ladder  = {C in [1m, 5m, 15m, 1h, 6h, 1d] : C > w, w | C, 2C ≤ W}
readers = {w_q : w_q > w, w | w_q, w_q ≤ W}
chains  = maximal sequences w = c_0 < c_1 < ... < c_k over ladder ∪ readers
          with c_{i-1} | c_i
covers  = every prefix (c_k..c_0), k ≥ 1, of every chain, deduplicated
```

Each level `c_i` is built by `SummaryMergeWindows[c_i]` from `c_{i-1}`; a
level that appears in several chains is one group with one candidate per
narrower level it is built from.

**Mean windows by grid.** For a cover over levels `L` with range `W` run on
evaluation times `t ≡ phase (mod p)`:

```
mean(L, W, p, phase) = average over t = phase + k·p, k = 0 .. lcm(c_0..c_k, p)/p − 1,
                       of |cover_windows(t − W, t, L)|
```

Example: `L` = (1 min, 10 s), `W` = 5 min: on q1's 10 s grid 9.17 windows;
on q2's 60 s grid 5.

**Predicted plans.**

| Workload | Today (model) | After (predicted) |
| --- | --- | --- |
| 1 min range, q1 every 10 s, q2 range step 60 s | 1.0178e3 /s; q2 merges 6 KLL windows per series per step (368/s) | q2 reads `TimeRange 60s` over a 1 min level from the 10 s chain: 1 state per series, passed through (~10/s); the level costs ~1,000 merges/s at ingestion (~10/s): about 6.6e2 /s, −35% |
| 5 min range, same readers | 1.3658e3 /s; q2's cover charged 9.17 windows | q2's cover charged 5 windows: its merge ~1.3e2 /s instead of 2.4e2 /s; same plan, cost ~1.26e3 /s |

Memory: a reader level keeps `retention / w_q` windows per series, the same as
the ladder level of that width (1 min: 120 windows per series over 2 h,
1.29 GB at 10,000 series for DDSketch in the second example).

**Exactness.** Unchanged from multi-level chains: a reader level holds the
same states as the chain of that width built from rows (exact for DDSketch,
Count-Min, Count Sketch, HLL, KMV, exact accumulators, counters; KLL keeps
its rank bound). A reader reads a coarser chain only in a context on that
chain's grid (Stage 3's delivery check), so every window it reads ends on one
of its evaluation times' window boundaries.

**Complexity.** At most one reader level per reader; covers per prefix of each
divisibility chain (a handful per state group in practice). The Pass 2 check
goes from one gcd per group to one test per reaching query.

## 4. Code interface

```diff
 // crates/stage1-pass2-reuse-candidates/src/rules/chains.rs
-fn add_wider_levels(memo: &mut Memo, state: MemoGroupId, path: &ChainPath, chain_groups: &[MemoGroupId]) -> Result<(), Pass2RuleError>;
+/// Ladder levels and reader levels (`reader_widths_ms`: each reader's own
+/// gcd(grid, range, offsets)) of `state`'s chains; covers per prefix of each
+/// divisibility chain.
+fn add_wider_levels(memo: &mut Memo, state: MemoGroupId, path: &ChainPath, chain_groups: &[MemoGroupId], reader_widths_ms: &BTreeSet<u64>) -> Result<(), Pass2RuleError>;

 // crates/stage1-pass2-reuse-candidates/src/evaluation_grids.rs
-pub(crate) fn chain_off_the_evaluation_grid(memo: &Memo, evaluation_grid_of_group: &[Option<EvaluationGrid>], group: MemoGroupId, input_group: MemoGroupId) -> Option<(u64, Option<u64>)>;
+/// The chain's width and the reaching queries' periods when it divides none
+/// of them (`evaluation_period_ms_per_query` of `group`); `None` when some
+/// reader is on its grid.
+pub(crate) fn chain_off_every_readers_grid(memo: &Memo, evaluation_period_ms_per_query: &[BTreeMap<usize, Option<u64>>], group: MemoGroupId, input_group: MemoGroupId) -> Option<(u64, Vec<Option<u64>>)>;

 // crates/types/src/ir/operator/tumbling_window_chain.rs

 // crates/cost/src/node/usage/mod.rs
+/// The evaluation grid of the context a node runs in (the gcd of its
+/// readers' grids when shared); a cover's windows are averaged over it.
+pub evaluation_grid: Option<EvaluationGrid>,   // field of the per-node costing input
```

`RootPruneReason::ChainWidthDoesNotDivideEvaluationGrid` keeps its name and
lists the readers' periods. Stage 3 and the executor need no change: Stage 3
already selects per context, and the executor already runs
`SummaryMergeWindows` levels and covers.

## 5. Decisions (proposed)

1. **Levels, not separate chains from rows.** A coarse reader reads a level
   merged from the narrow chain, not a second chain built from the rows,
   which would add a row update per series per sample (100,000 per second
   here) to save a few merges per step. Both remain candidates where the
   closure already has the from-rows chain; Stage 3 decides.
2. **Per-reader check in Pass 2, per-context selection in Stage 3.** Rejected:
   keeping the group-level gcd and forcing every reader to the finest width
   (today), and splitting the state group per reader in the memo (the
   readers share everything else).
3. **`w_q ≤ W` for reader levels.** A reader level is exact for its reader at
   every step, so one window per step already pays; the ladder's `2C ≤ W`
   rule is about readers off the level's grid.
4. **Cover cost per context grid.** The cost a node is charged depends only on
   its own context, as for every other node.

## 6. Open questions

0. Cost model 3 (`lowest_mean_latency`) leaves levels and covers out on
   main until its model over them is bounded; reader levels follow the
   same rule.

1. Cap on covers when readers' widths form many divisibility chains (none of
   the corpora has more than two readers per state group).
2. Step parallelism in cost model 3's latency is in progress elsewhere; this
   proposal changes only the work per step.

## 7. Test plan (failing tests first)

| # | Test (crate) | Checks | Fails today because |
| --- | --- | --- | --- |
| W1 | (done on main: `closed_form_means_equal_counted_means`) | `(1 min, 10 s)` levels, `W` = 5 min: 9.17 windows on a 10 s grid, exactly 5 on a 60 s grid; phase 30 s on a 60 s grid: 4 + 6 = 10. | The function averages over the narrowest width only. |
| W2 | `a_coarse_reader_gets_its_own_level` (stage1-pass2) | `m[1m]` read every 10 s and every 60 s: the 60 s chain group has a `SummaryMergeWindows` candidate from the 10 s chain; the state group has a merge over `TimeRange 60s` of it, and its edge is kept. | Only the 10 s chain serves; the 60 s edge fails `ChainWidthDoesNotDivideEvaluationGrid`. |
| W3 | `a_coarse_chain_serves_only_contexts_on_its_grid` (stage3) | `m[30s]` read every 10 s and every 15 s: Pass 2 keeps the 10 s and 15 s chains' edges (each on one reader's grid); Stage 3 offers the 15 s chain only in the 15 s reader's context and the 10 s chain only in the 10 s reader's. | Pass 2 prunes both edges: only the 5 s chain (the gcd grid) serves. |
| W4 | `reader_levels_nest_by_divisibility` (stage1-pass2) | Readers every 60 s and every 90 s over `m[1h]` (base 30 s): covers over the prefixes of 30 s → 1 min → 5 min → 15 min and over (90 s, 30 s); none holding both 60 s and 90 s. | No reader levels. |
| W5 | `a_single_reader_gets_no_reader_level` (stage1-pass2) | `m[1m]` every 60 s alone: chain 60 s, no `SummaryMergeWindows`. | Passes today; guards against regressions. |
| W6 | (done on main: `a_cover_is_charged_on_its_readers_grid`) | The 5 min example: q2's cover node charged 5 windows per step (`5.00`), q1's 9.17. | Both charged 9.17. |
| W7 | `stage3_gives_a_coarse_reader_its_level` (integration) | The 1 min example: q2's context reads `TimeRange 60s` over a `SummaryMergeWindows[60 s]`; plan cost below 1.017759e3 /s; q1's context still reads the 10 s chain. | No such candidate. |
| W8 | `coarse_reader_answers_equal_fine_answers` (executor or planner e2e) | The 1 min example installed and run: q2's answers at 60 s steps equal those read from the 10 s chain (exact sums and DDSketch p99, bit for bit); q1 unchanged. | The plan does not exist. |
| W9 | Plan snapshots (devtools) | Only plans with a cover, or a state read on two grids, change; each listed with old and new cost. | Expected diff, reviewed. |
