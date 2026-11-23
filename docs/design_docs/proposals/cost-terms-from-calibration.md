# Cost Terms From Calibration

**Status:** Section 1 implemented, in `plan_cost` and Stage 3's
selection. Sections for the result cache's memory, the fixed CPU per
refresh run and the per-step query cost follow, one per change.

**Audience:** Implementers of the cost model (`asap-cost`) and of
deployment calibration (`tools/calibration_gate/`).

A calibration of a data plane against the planner's predictions (the C10
gate of `stage3-lowest-mean-latency.md`, section 6.2) found four costs
the cost inputs cannot express. This document adds them, one section
each: the components (what is measured and what it means), the algorithm
(how the cost model charges it) and the code interface.

## 1. Resident view windows

### Example

`demo.dashboard-heavy` at 10,000 series (`quantile_over_time(0.99,
data[1h])`, `topk(10, sum by (region, service, job) (rate(data_total[1h])))`,
`max_over_time(data[30m])`, `sum by (region, service)
(increase(data_total[1h]))`, every 15 s) keeps a 10 s DDSketch chain of
`data` for 1 h: 360 windows of 10,000 states. The planner charges every
retained window as memory, 3,880 MiB for that chain and 5,475 MiB for the
plan. The measured data plane held a working set of 570 MiB, of which
308 MB were decoded view windows, 74 of them, and 1.9 GB of view files.

The data plane keeps every window of a view in a file and a cache of
decoded windows in memory: a window enters the cache when it is written or
read, and leaves it when it was not read for an idle time (60 s by
default). The quantile reads its hour as a cover over 1 min, 5 min and
15 min levels and the 10 s chain, so each evaluation reads only the 10 s
windows at the two edges of its hour: the middle 10 s windows are read by
no one and leave memory.

### Components

- **The window cache** (`DataPlaneExecution::view_window_cache`): its idle
  time `idle_ms`. Absent: every retained window is memory and none is
  disk, as before.
- **A chain's readers**: each reader of a stored view with tumbling
  windows (`windows_readers_need`), with what it reads at time `t`:
  - `TimeRange(range)` over shifts of `offset` in all: the windows ending
    in `(t − offset − range, t − offset]`; a read of the current time only
    (`range` 0), the newest window ended by `t − offset`;
  - a `TimeRangeCover` of `range` over levels `w_0 > … > w_k`: the windows
    of this view's level that `cover_windows(t − range, t, w)` takes;
  - a reader the cost model does not follow: every retained window.
- **When it reads**: a query-time reader at each request of each query
  reaching it (its evaluation grid; a range request at each of its steps),
  each query one reader; an ingestion-time reader (a wider level built
  from this one) at each close of its own window.

### Algorithm

One reader alone needs the windows its reads reach back over retained:
its offset, its range and its request's older steps, in windows of the
view's width `w`, at least one, and one more for a read of the current
time only (`ChainReader::retained_windows`). At a time `now` on the view's
grid, the reader keeps resident those of its retained windows

- written in `(now − idle, now]`, and
- read by it at a time in `(now − idle, now]`.

Its resident windows are the mean count over one period of the reader
(the least common multiple of `w`, its period and its cover's widths;
`now` sampled at most 2,048 times), and the most.

A view retaining `N` windows keeps resident the most of its readers'
counts, and at least the windows written within the idle time of its `N`.
Every retained window is in its file: disk, `N` windows. Memory is its
resident windows (the mean for the average and cost model 1, the most for
the peak) and the window being built.

The most over readers is exact when the largest reader's reads hold the
others' (readers up to the newest window, the usual case). It is below
the union when readers read disjoint windows: `max_over_time(m[5m] offset
1h)` and `max_over_time(m[5m])` every 15 s keep 24 and 20 windows; the
data plane holds both sets, 44, and the cost model charges 24. The most,
not the union, is what Stage 3's selection can charge as a linear program,
and `plan_cost` charges the same.

Examples (15 s windows, idle 60 s):

| Reader | Resident of what it needs retained |
| --- | --- |
| `max_over_time(m[5m] offset 1h)` every 15 s | 24 of 260: the 20 retained of its last minute's reads, and the 4 newest |
| `max_over_time(m[1h])` every 15 s | all 240 |
| a 1 h cover over 5 min and 15 s, at `now` = 100 h + 105 s | 23 of 245 15 s windows: 7 at the right edge, 16 at the left |
| a 5 min level built at each 5 min close | mean 7.2, most 20 of 20 |
| the current time an hour back, every 15 s | 5 of 241: the one ended by `now − 1 h`, the 4 newest |
| none | the 4 newest |

**Selection.** Stage 3 charges the same. A cached view's execution and
its $\pi_v$ charge its retained windows as disk, and a variable $r_v$
per cached view holds its resident windows, charged at its memory cost
per window (in cost model 3's memory row, its bytes per window):

$$\text{(W)}\quad r_v \ge \operatorname{res}_e \cdot \mathrm{read}_e$$

for each read $e$ of $v$'s executions, an edge $y$ or a query output $z$:
$\operatorname{res}_e$ is the most any reader above the edge keeps
(`windows_readers_need` over candidates, each query reaching a
query-time reader on the edge consumer's grid one reader), and at least
the windows written within the idle time of those the read's retention
need keeps. $r_v$ costs more the larger it is, so at the optimum it is
the most over the selected reads, what `plan_cost` charges. The mean
counts are in cost model 1's objective, the most in cost model 3's memory
row.

On the dashboard above, the plan's predicted memory goes from 5,475 MiB
to 292 MiB against a measured 570 MiB working set (the rest of
which the cost model does not price: the process, raw-data panes, the
result cache of section 2).

**Limits.** Without an answer cache (section 2) a range request reads its
whole span at each request, so a reader's windows reach back its range
plus its span; a plan retaining less (a data plane caching final results)
is then charged more resident windows than it retains. Above a
pass-through reader (a `TimeShift`), Stage 3 takes
each candidate that may read there, as (R) does, and charges their most;
the plan's own readers there may keep fewer. Cold windows
read back are not charged their decode CPU: at steady state a periodic
reader reads only resident windows.

### Code interface

`crates/cost/src/model.rs`:

```rust
pub struct DataPlaneExecution {
    pub ingest_threads: Option<u32>,
    pub precompute_threads: u32,
    pub query_threads: u32,
    pub allowed_lateness_ms: u64,
    pub max_thread_utilization: f64,
    pub ingest_work_at_refresh: bool,
    /// The data plane's cache of decoded view windows; `None` (default):
    /// every retained window is memory.
    pub view_window_cache: Option<ViewWindowCache>,
}

pub struct ViewWindowCache {
    /// Milliseconds after its last write or read at which a window leaves
    /// memory.
    pub idle_ms: u64,
}
```

In JSON: `"data_plane_execution": {"view_window_cache": {"idle_ms": 60000}}`.

`crates/cost/src/node/usage/mod.rs`: under the cache an in-memory
ingestion-time view's node charges its retained windows as disk and its
window being built as memory, and reports `cached_windows:
Option<CachedViewWindows { bytes_per_window, width_ms, retained }>`.

`crates/cost/src/plan/resident_windows.rs`: `ChainReader { read:
WindowRead, times: ReadTimes }` with `retained_windows(width_ms)`;
`reader_resident_windows(width_ms, idle_ms, reader)`,
`written_resident_windows(width_ms, retained, idle_ms)` and
`view_resident_windows(width_ms, retained, idle_ms, readers)`, each
`(mean, most)`. `plan_cost` charges each cached view
`view_resident_windows` of its readers (`resident_view_windows` in
`crates/cost/src/plan/cost.rs`).

`crates/stage3-plan-selection/src/resident_windows.rs`:
`resident_reads(...) -> Vec<ResidentRead>`, the windows each read keeps
(`ExecutionCosts::resident_reads`, with `per_resident_window` per view);
`crates/stage3-plan-selection/src/milp/retention.rs`:
`resident_window_terms` ($r_v$ and (W), in both MILPs) and
`resident_windows_quantity` (a selection's, in
`selection_cost_per_second`).

### Tests

| Test | Checks |
| --- | --- |
| `resident_windows` unit tests (`asap-cost`) | the six examples above, and the most of two readers |
| `c11_a_window_cache_keeps_only_recently_read_windows_in_memory` (integration) | with the cache, the offset chain's memory falls below a fifth and every retained window is disk; without it, as before |
| `the_objective_is_the_plan_cost_with_a_window_cache` (integration) | the MILP's optimum is `plan_cost` with the cache: an offset and a recent reader of one chain, a 1 h quantile, a 1 h range |
| `a3_under_a_window_cache_the_memory_limit_counts_resident_windows` (integration) | cost model 3's memory row is the plan's peak: a limit at it keeps the chain, one byte below selects the raw read |

