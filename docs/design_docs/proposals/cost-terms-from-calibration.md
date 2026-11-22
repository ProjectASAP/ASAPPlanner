# Cost Terms From Calibration

**Status:** Section 1 implemented. Sections for the result cache's memory,
the fixed CPU per refresh run and the per-step query cost follow, one per
change.

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

The data plane keeps a view's windows in files and a cache of decoded
windows in memory: a window enters the cache when it is written or read,
and leaves it when it was not read for an idle time (60 s by default),
or when the cache is over a byte bound. The quantile reads its hour as a
cover over 1 min, 5 min and 15 min levels and the 10 s chain, so each
evaluation reads only the 10 s windows at the two edges of its hour: the
middle 10 s windows are read by no one and leave memory.

### Components

- **The window cache** (`DataPlaneExecution::view_window_cache`): its idle
  time `idle_ms`, and an optional bound `max_bytes`. Absent: every
  retained window is memory, as before.
- **A chain's readers**: each reader of a stored view with tumbling
  windows (`windows_readers_need`), with what it reads at time `t`:
  - `TimeRange(range)` over shifts of `offset` in all: the windows ending
    in `(t − offset − range, t − offset]`;
  - a `TimeRangeCover` of `range` over levels `w_0 > … > w_k`: the windows
    of this view's level that `cover_windows(t − range, t, w)` takes;
  - a reader the cost model does not follow: every retained window.
- **When it reads**: a query-time reader at each request of each query
  reaching it (its evaluation grid; a range request at each of its steps);
  an ingestion-time reader (a wider level built from this one) at each
  close of its own window.

### Algorithm

For a view of window width `w` retaining `N` windows (its retention over
`w`), at a time `now` on its grid, the resident windows are those of the
`N` newest

- written in `(now − idle, now]`, and
- read by any reader at a time in `(now − idle, now]`.

The view's memory is its bytes per window times the mean count over one
period of its readers (the least common multiple of its width, their
periods and their covers' widths; `now` sampled at most 2,048 times), plus
the window being built, as before; its peak uses the most. The `N − mean`
other windows are disk. With `max_bytes`, resident bytes past the bound
are disk too.

Examples (15 s windows, idle 60 s):

| Reader | Resident of retained |
| --- | --- |
| `max_over_time(m[5m] offset 1h)` every 15 s | 24 of 260: the 20 retained of its last minute's reads, and the 4 newest |
| `max_over_time(m[1h])` every 15 s | all 241 |
| a 1 h cover over 5 min and 15 s, at `now` = 100 h + 105 s | 23 of 245 15 s windows: 7 at the right edge, 16 at the left |
| a 5 min level built at each 5 min close | mean 7.5, most 23 of 241 |
| none | the 4 newest |

On the dashboard above, the plan's predicted memory goes from 5,475 MiB
to 304 MiB against a measured 570 MiB working set (the rest of which the
cost model does not price: the process, raw-data panes, the result cache
of section 2).

**Limits.** Stage 3's selection still charges every retained window (its
memory per execution is per view, not per reader); the report and
`--price-plan` charge the resident ones. A range request of 241 steps
reads its whole span at each request here; a data plane that keeps each
step's answer computes only its new steps (section 2). Cold windows read
back are not charged their decode CPU: at steady state a periodic reader
reads only resident windows.

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
    /// The most bytes of windows the cache holds; `None`: no bound.
    pub max_bytes: Option<u64>,
}
```

In JSON: `"data_plane_execution": {"view_window_cache": {"idle_ms": 60000}}`.

`crates/cost/src/plan/resident_windows.rs`: `resident_window_ends(width_ms,
retained, idle_ms, now_ms, readers)` and `resident_windows(width_ms,
retained, idle_ms, readers) -> (mean, most)`, over `ChainReader { read:
WindowRead, times: ReadTimes }`. `plan_cost` applies them to each
in-memory ingestion-time view (`resident_view_windows` in
`crates/cost/src/plan/cost.rs`).

### Tests

| Test | Checks |
| --- | --- |
| `resident_windows` unit tests (`asap-cost`) | the five examples above, and none |
| `c11_a_window_cache_keeps_only_recently_read_windows_in_memory` (integration) | with the cache, the offset chain's memory falls below a fifth, and the memory it no longer holds is charged as disk byte for byte; without it, as before |
