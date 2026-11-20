# Stage 3: Lowest Mean Latency Under Cluster Limits

**Status:** Design proposal. Design only: no code or tests yet. The test plan
(section 6) is approved before tests are written; the implementation follows
the tests.

**Audience:** Planner designers and implementers of plan selection (Stage 3)
and the cost model (`asap-cost`).

## Example

A deployment runs on its own cluster (cost model 3) and asks for the plan
with the lowest latency its users see. One query,
`max_over_time(m[5m])`, is evaluated every 10 s. Stage 2 offers two plans
(illustrative times):

| Plan | Work | CPU per second | What a query waits for |
| --- | --- | --- | --- |
| A: query-time scan | each evaluation scans 5 min of raw rows: 30 ms | 30 ms × 0.1/s = 3 ms/s | 30 ms of execution |
| B: 1 min tumbling-window chain | each window close refreshes the chain: 44 ms; each evaluation merges 5 windows: 2 ms | 44 ms ÷ 60 s + 2 ms × 0.1/s ≈ 0.93 ms/s | the newest window's refresh (44 ms), then 2 ms |

Cost model 1 picks B, the cheaper plan. A deployment that wants low latency
wants A: B's evaluation cannot answer before the newest window is refreshed,
so its latency is 46 ms, not 2 ms. Today Stage 3 cannot express either point:

1. `select_plan` refuses cost model 3 (`ErrorSelectingPlan::UnsupportedCostModel`):
   the selection MILP implements cost model 1 only.
2. `asap_cost::QueryLatency` counts the query-time work and the compaction
   of ingestion-time partial copies, but not the wait for the newest
   window's refresh, its queueing behind other refreshes, or the allowed
   lateness the data plane waits before closing a window.
3. A query's `max_latency_ms` is checked by no cost model:
   `RejectionCause::LatencyBoundExceeded` exists, but no MILP row enforces it.
4. The per-node times the cost model predicts are not yet checked against a
   running data plane. A comparison of predicted against measured per-phase
   times found precompute under-predicted about 2× (a fixed cost of about
   44 ms per refresh is not modelled), `max_over_time` query runs
   under-predicted 2.6× (a fixed 3–7 ms per run), and DDSketch merges
   over-predicted (the value range defaults to 1..1e9). An objective built
   on uncalibrated times would rank plans by errors.

This document adds `ClusterObjective::LowestMeanLatency`, the latency
model it needs, the calibration that comes first, and the MILP rows that
select under the cluster's limits.

## 1. Components

### 1.1 The objective

`ClusterObjective::LowestMeanLatency` minimizes the frequency-weighted
mean query latency over the plans that fit the cluster's limits:

$$\bar L = \frac{\sum_q w_q L_q}{\sum_q w_q},$$

where $L_q$ is query $q$'s latency per request in milliseconds (section 2.1)
and $w_q$ its evaluations per second:

| Recurrence | $w_q$ (evaluations per second) |
| --- | --- |
| `fixed_interval_at` with interval $I$ ms | $1000 / I$ |
| any recurrence with a `range_evaluation` | the request rate × `RangeEvaluation::evaluations_per_request` (each step is one evaluation, and each step waits for its whole request) |
| `estimated_rate` | the rate (× steps) |
| one-off, scheduled, unknown | `invocations / non_repeating_query_horizon_seconds`, as the cost model spreads them today |

$w_q$ is the query's own rate (`execution_demand::evaluation_rate` over that
query alone), not the batch rule `plan_cost` uses to count a shared node's
computations. Ties on $\bar L$ go to the smaller peak memory, then the
smaller disk (section 2.5).

`ClusterObjective::LowestLatency` (worst query) and `LeastMemory` keep
their meaning (section 2.7).

### 1.2 Hard constraints

A plan is valid under `LowestMeanLatency` only when all of these hold:

| Constraint | Row |
| --- | --- |
| Each query's latency bound | $L_q \le$ `max_latency_ms` of $q$, when set |
| Memory | peak memory $\le$ `memory_bytes` |
| Disk | disk bytes $\le$ `disk_bytes` |
| CPU (when `cpu_cores` is set) | average cores used: $\sum$ CPU core-seconds per second (ingest, precompute, query, request overhead) $\le$ `cpu_cores` |
| Precompute load | $\rho \le \rho_{\max}$ (section 2.3) |

Peak memory and disk are `ResourceUsage::peak_memory_bytes` and
`ResourceUsage::disk_bytes` as `plan_cost` defines them today. Both are sums
over the plan's nodes (plus each raw-data source once, for disk), so they are
linear in the selection variables.

### 1.3 Data-plane execution parameters

The latency model needs facts about how the deployment's data plane runs
work. They are deployment parameters, passed in, never inferred:

| Parameter | Unit | Default | Meaning |
| --- | --- | --- | --- |
| `ingest_threads` | threads | `None`: derived as today | threads inserting arriving rows; each builds a partial copy of an in-memory state's newest window |
| `precompute_threads` | threads | 1 | threads running window-close refreshes of ingestion-time views |
| `query_threads` | threads | 1 | threads running query requests; one request runs on one thread |
| `allowed_lateness_ms` | ms | 0 | how long the data plane waits after a window's end before closing it |
| `max_thread_utilization` ($\rho_{\max}$) | fraction | 0.7 | the largest precompute load a valid plan may have (hard); also the target utilisation of the recommended thread counts (section 2.6) |

**Where they live: `CostInputs::data_plane_execution`**, a new struct beside
`resource_speed`. Reasons:

- `CostInputs` already holds how fast and how the deployment runs work
  (`resource_speed`, `non_repeating_query_horizon_seconds`, the measured
  table). Thread counts and lateness are the same kind of fact: they change
  how long work takes, not which work exists.
- Not `DeploymentCapabilities`: Stage 2 reads it to decide which candidates
  exist (exclusions, retention support). Thread counts never add or remove a
  candidate.
- Not `ClusterLimits`: it exists only under cost model 3, but the latency
  breakdown is reported under every cost model (section 2.6), and the
  calibration (section 2.4) compares predicted to measured times under any
  cost model. `ClusterLimits` keeps only the cluster's resource limits and
  objective.
- $\rho_{\max}$ sits with the thread counts because $\rho$ is defined over
  `precompute_threads` and the recommended thread counts are sized by it.

Of the thread counts, only `precompute_threads` enters selection (through
$\rho$ and $Q_G$); `ingest_threads` sets the partial copies; `query_threads`
is not a constraint. Ingest and query CPU are not limited against their
thread counts: the data plane configures its threads from the planner's
recommended configuration instead (section 2.6).

`ingest_threads: None` keeps today's rule (one partial copy per core the
ingestion work needs), so cost model 1 plans and costs do not change.

### 1.4 Precompute and refresh groups

An ingestion-time node's work splits by when it runs:

- **Ingest work**, per arriving row, on the ingest threads: a `Scan`,
  `Filter` or `Project` at ingestion time, a `SummaryAgg` or
  `SummaryTumblingWindowChain` inserting a row into its window's state, an
  `Aggregate` updating its values.
- **Refresh work**, once per window close, on the precompute threads:
  merging the ingest threads' partial copies of the closed window (today's
  `QueryLatency::compaction_ms`), and every ingestion-time node that reads a
  closed window (a `SummaryMerge`, `SummaryEstimate`,
  `FinalizeExactAccumulator` over it, or a chain over a smaller window's
  output), plus each node's fixed per-refresh cost (section 2.4).

A **refresh group** is the set of selected ingestion-time nodes with one
`RefreshWindow` (`width_ms`, `phase_ms`): their windows close at the same
instants and one task refreshes them, on one precompute thread. Its
**refresh time** $P_G$ in ms per window is the sum of its nodes' refresh
work per window.

### 1.5 Calibration inputs

Calibration uses the existing inputs plus one new field:

- `MeasuredOperationCosts::operators[i].ns_per_invocation`: per evaluation
  at query time (the fixed 3–7 ms per run), as today.
- `MeasuredOperationCosts::ns_per_evaluation`: per request, as today.
- New `OperatorCost::ns_per_refresh` (default 0): CPU ns per window close of
  an ingestion-time node, the fixed cost per refresh (the ~44 ms). Today's
  `ns_per_invocation` at ingestion time is per second of arriving rows and
  cannot express it.
- `DataWorkload` `column_magnitudes`: a declared value range replaces the
  DDSketch default 1..1e9 (reported today as an `AssumedDataFact`).
- `CostInputs::frozen_calibration_factor`: the scale for the formula-priced
  nodes the table does not cover.

What the current format can and cannot express:

| Measured term | Format today | Change |
| --- | --- | --- |
| time per unit of an operator, per phase and family | `ns_per_unit` | none |
| fixed time per query run of an operator | `ns_per_invocation` with `phase: query_time` | none |
| fixed time per request | `ns_per_evaluation` | none |
| fixed time per precompute refresh (window close) | none: `ns_per_invocation` at ingestion time is per second of arriving rows | new `ns_per_refresh` |
| scale for uncovered nodes | `frozen_calibration_factor` | none |

A calibration file is a `CostInputs` JSON (`asap_planner --cost-inputs`)
carrying the measured table and the factor, e.g.:

```json
{
  "measured_operation_costs": {
    "ns_per_evaluation": 1500000,
    "operators": [
      {"operator": "SummaryTumblingWindowChain", "phase": "ingestion_time",
       "sketch": {"Kll": {"k": 200}}, "ns_per_unit": 310, "ns_per_refresh": 44000000},
      {"operator": "Aggregate", "phase": "query_time", "measure": "max",
       "ns_per_unit": 9, "ns_per_invocation": 5000000}
    ]
  },
  "frozen_calibration_factor": 1.8
}
```

A file without `ns_per_refresh` loads as before (default 0).

## 2. Algorithm

### 2.1 Query latency

$$L_q = F_q + E_q$$

**Freshness wait** $F_q$: before $q$ can answer, the newest window of every
ingestion-time view it reads must be refreshed:

$$F_q = \max\Big(a,\ \max_{G \text{ read by } q} \big(a + Q_G + P_G\big)\Big)$$

- $a$ = `allowed_lateness_ms`. Every query that reads data waits for it,
  also one reading only raw rows: no row of the newest window is complete
  before it.
- $P_G$: the refresh time of group $G$ (section 1.4).
- $Q_G$: the precompute queueing time of $G$, the v1 linear upper bound:
  the refresh times of the other groups that can close at the same instant
  as $G$, divided by `precompute_threads`. Two groups
  $(W_1, \phi_1)$, $(W_2, \phi_2)$ can close at one instant when
  $\phi_1 \equiv \phi_2 \pmod{\gcd(W_1, W_2)}$. A group that $G$ reads from
  (a 5 min view over 1 min windows) is counted in full, not divided: $G$
  cannot start before it ends.

$$Q_G = \frac{1}{\text{precompute\_threads}} \sum_{H \ne G,\ H \text{ coincides with } G,\ G \text{ does not read } H} P_H \;+\; \sum_{H \text{ read by } G} P_H$$

**Query execution** $E_q$: the critical path of $q$'s query-time sub-DAG for
one request, plus the request's fixed overhead (`ns_per_evaluation`). One
request runs on one query thread (the executor's run is single-threaded), so
every node of the sub-DAG is on the critical path: $E_q$ is the sum of the
per-run times $d_e$ of the query-time nodes $q$ reads, each node once.
A shared `OneEvaluation` node is computed once per evaluation time by its
first reader and waited on by the others; it is counted on every reader's
path (each reader may be the first). A query-time `AcrossEvaluations` node
counts the work of one run (what this evaluation adds), as `plan_cost`
charges it.

This keeps today's `QueryLatency::query_ms` rule (each reached query-time
node once) and moves `compaction_ms` into the refresh time it belongs to.

### 2.2 Weights

$w_q$ is section 1.1's table, computed once per query before the MILP. The
equivalent-context copies (`copies_in_equivalent_contexts`) share the
original query's selection, so their latencies are equal: the original's
weight is the sum of its copies' weights and the copies get no rows.

### 2.3 Precompute load

$$\rho = \frac{1}{\text{precompute\_threads}} \sum_G \frac{P_G / 1000}{W_G / 1000}$$

the fraction of the precompute threads' time spent refreshing ($P_G$ in ms
per window, $W_G$ the window width in ms). The hard constraint
$\rho \le \rho_{\max}$ (default 0.7) keeps the refresh backlog from growing.
$\rho$ is linear in the selection: $P_G$ is a sum over selected nodes.

Within $\rho \le \rho_{\max}$, the queueing time $Q_G$ is the conservative
linear bound of section 2.1 (decided for v1): it ignores
work left over from earlier closes, which $\rho \le \rho_{\max}$ keeps small,
and assumes every coinciding group is ahead of $G$.

### 2.4 Calibration (before the objective)

The objective ranks plans by predicted times, so those times are checked
against a running data plane first. The procedure is generic: any
deployment that reports per-phase times can run it.

1. **Predict.** Plan a calibration workload and write the plan with its
   report. The report gives, per node, its predicted time per phase:
   ingest CPU per second, refresh ms per window close, query ms per run
   (section 3.3), and per query its latency breakdown.
2. **Measure.** The deployment runs the plan and records the same
   quantities per node and phase: ingest CPU per second, refresh time per
   window close, time per query run, time per request outside operators.
3. **Compare.** A comparison tool joins predicted and measured by node id
   and phase and reports the ratio per node, operator kind and summary
   family.
4. **Feed back**, by cause:
   - a fixed cost per run or per refresh: `ns_per_invocation` (query time)
     or `ns_per_refresh` (ingestion time) of that operator;
   - a per-unit error: `ns_per_unit` of that operator and family;
   - a fixed cost per request: `ns_per_evaluation`;
   - a wrong data assumption (an `AssumedDataFact`, e.g. the DDSketch value
     range): declare the fact in the data workload;
   - nodes the table does not cover: `frozen_calibration_factor`.
5. **Repeat** until every node's predicted time per phase is within the
   acceptance ratio of the measured one: 1.5× (decided), per node and per
   phase, on the calibration workloads. The objective is implemented only
   after the calibration passes.

The deployment supplies the measured costs as a `MeasuredOperationCosts`
table; the planner reads only that table.

### 2.5 Selection MILP

The rows are added to the existing selection MILP (variables $x$, $y$, $z$,
$t$, $h$, $r$, $a$ and constraints (A)–(F), (R), (X), (K) unchanged).
Times are in ms, constants are computed per execution before the MILP.

**Per-execution constants.** For execution $e$: $d_e$, its query-time
time per run; $p_e$, its refresh time per window (ingestion time); $m_e$,
its peak memory; $k_e$, its disk; $c_e$, its CPU core-seconds per second.
An edge's share (the producer-dependent part, as cost model 1 puts on $y$)
gets the same split.

**Reads.** $a_{q,e} \in [0,1]$: query $q$'s selected sub-DAG reads
execution $e$. Today (X) creates $a_{q,e}$ only where it needs it; the
latency rows need it on every execution reachable from $q$'s answering
executions in $q$'s context:

$$a_{q,e} \ge z_{q,e}, \qquad a_{q,u} \ge a_{q,e} + y_{u \to e} - 1.$$

Lower bounds suffice: the objective and the latency rows push $a$ down,
and an $a$ at 1 where $q$ does not read only raises $L_q$. Query-time reads
stop at an ingestion-time execution (it starts a freshness term).

**Refresh groups.** Per group $G$: $P_G = \sum_{e \in G} p_e\, x_e$ (a
linear expression, no variable), and $f_G = a + Q_G + P_G$ with $Q_G$ from
section 2.1, also linear.

**Latency.** Per query, continuous $F_q, L_q \ge 0$:

$$F_q \ge a, \qquad F_q \ge f_G - M_G\,(1 - a_{q,e}) \quad \text{for each ingestion-time } e \in G \text{ reachable by } q,$$

$$L_q = F_q + \sum_{e \text{ query-time}} d_e\, a_{q,e} + \text{request}_q.$$

The max over refresh groups is linearized with $F_q$ and big-M:
$M_G$ is $f_G$ with every selectable execution selected, an upper bound
computed before the MILP (not a large constant, so the rows stay tight).

**Constraints.** $L_q \le$ `max_latency_ms`;
$\sum_e m_e x_e + \ldots \le$ `memory_bytes`;
$\sum_e k_e x_e + \sum_s r^{\text{bytes}}_s \le$ `disk_bytes` (raw data
once per source, as the $r_s$ rows do for cost); $\sum c_e x_e + \ldots \le$
`cpu_cores`; $\frac{1}{\text{precompute\_threads}} \sum_G P_G / W_G \le
\rho_{\max}$.

**Objective.** Minimize $\sum_q w_q L_q$ (the mean's numerator;
$\sum_q w_q$ is fixed).

**Sharing.** An execution read by several queries contributes $d_e$ to each
reader's $L_q$ through its own $a_{q,e}$, and its CPU, memory and disk once
(through $x_e$). Refresh groups are shared by construction.

**Tie-breaks: lexicographic second solves.** Solve 1 minimizes
$\sum w_q L_q$, giving $L^*$. Solve 2 adds
$\sum w_q L_q \le L^* (1 + \epsilon)$ and minimizes peak memory, giving
$M^*$. Solve 3 adds peak memory $\le M^*(1+\epsilon)$ and minimizes disk.
$\epsilon$ is the existing relative cost tolerance (`COST_RELATIVE_TOLERANCE`).
Each later solve starts from the previous solution. Weighted tie terms
(latency + λ·memory + μ·disk in one objective) are rejected: latency in ms,
memory and disk in bytes differ by up to 12 orders of magnitude, and with
the objective scaled to a largest coefficient of 1e6 and HiGHS's absolute
tolerance of 1e-6, a λ small enough never to override a real latency
difference falls under the tolerance, so ties would be broken by HiGHS's
search path, the instability the 1e6 scaling was introduced to remove.
`Selection::milp_solve_count` counts the extra solves.

**Scaling.** The objective is scaled as today: the largest objective
coefficient becomes 1e6. With continuous $L_q$, the scale is taken over the
largest per-execution contribution $w_q d_e$ and $w_q M_G$; the constraint
rows stay in ms and bytes, unscaled.

**Pruning before the MILP.**
- `dearer_than_a_feasible_plan` (the greedy cost upper bound) is cost model
  1's and is not applied under cost model 3 in v1.
- `dominated_by_one_evaluation_twin` is applied with its condition restated
  over the cluster quantities: the twin is no slower ($d$, $p$), uses no
  more peak memory, disk and CPU, and answers the same outputs.
- An execution whose own $d_e$ (or freshness lower bound $a + p_e$) is above
  the `max_latency_ms` of every query that may read it is left out.

### 2.6 Report

Every cost model reports, per query, the latency breakdown that cost model
3 optimizes (section 3.3): allowed lateness, precompute queueing, precompute
execution, query execution, their sum, and the weight. `PredictedUsage`
gains the precompute load $\rho$. Under cost model 3,
`Selection` reports the selected plan's mean latency. As for cost model 1's
objective today, the MILP's mean latency equals the mean latency `plan_cost`
computes for the selected plan.

**Recommended configuration.** The report also states, per thread role,
the CPU the selected plan is predicted to use and the threads a deployment
needs to serve it at the target utilisation $\rho_{\max}$:

| Role | Predicted CPU core-seconds per second | Recommended threads |
| --- | --- | --- |
| ingest | ingest work of the ingestion-time nodes | $\lceil \text{cores} / \rho_{\max} \rceil$, at least 1 |
| precompute | $\sum_G P_G / W_G$ (refresh work) | $\lceil \text{cores} / \rho_{\max} \rceil$, at least 1 |
| query | query-time work plus request overhead | $\lceil \text{cores} / \rho_{\max} \rceil$, at least 1 |

Example: refresh work of 0.9 core-seconds per second at $\rho_{\max} = 0.7$
recommends 2 precompute threads (0.9 / 0.7 = 1.29, rounded up).

`precompute_threads` stays an input: $\rho$ and $Q_G$ are computed with it.
For a valid plan the recommendation is at most the input (since
$\rho \le \rho_{\max}$). A deployment may set `precompute_threads` from the
recommendation and replan: fewer threads raise $Q_G$ and may change the
selection, more threads admit plans with heavier refresh work. The
recommendation is stable when replanning with it as the input returns it
again. The recommendation is reported under every cost model.

### 2.7 Errors and the other objectives

**No plan fits.** When the MILP is infeasible under cost model 3, Stage 3
finds which limits block: for each limit family (memory, disk, CPU,
precompute load, each query's latency bound) it solves once without that
family and, if that is feasible, once more minimizing that family's
quantity. The error lists each limit whose removal alone admits a plan, with
the least that plan needs and the limit, e.g. "memory: the least any plan
needs is 3.2 GB, the limit is 2 GB". These solves run only on the error
path. When no single family is to blame, the error is today's
`Infeasible`, unchanged.

**`LowestLatency` and `LeastMemory`** fit the same rows: `LowestLatency`
minimizes $L_{\max}$ with $L_{\max} \ge L_q$ per query, then memory, then
disk; `LeastMemory` minimizes peak memory, then disk, then $L_{\max}$. This
work implements `LowestMeanLatency` only; the other two keep returning
`UnsupportedCostModel` until a follow-up enables them on these rows. Cost
model 2 keeps returning `UnsupportedCostModel`.

## 3. Code interface

### 3.1 Cost inputs (`asap-cost`, `model.rs`)

```diff
 pub enum ClusterObjective {
     /// The smallest worst-query latency; ties by peak memory, then disk.
     LowestLatency,
     /// The smallest peak memory; ties by disk, then worst-query latency.
     LeastMemory,
+    /// The smallest frequency-weighted mean query latency (each query
+    /// weighted by its evaluations per second); ties by peak memory, then
+    /// disk. JSON: `"lowest_mean_latency"`.
+    LowestMeanLatency,
 }

 pub struct CostInputs {
     pub cost_model: CostModel,
     pub resource_speed: ResourceSpeed,
     pub non_repeating_query_horizon_seconds: f64,
     pub measured_operation_costs: MeasuredOperationCosts,
     pub frozen_calibration_factor: Option<f64>,
+    /// How the deployment's data plane runs work: threads, allowed
+    /// lateness and the precompute load bound. Default: section 1.3.
+    pub data_plane_execution: DataPlaneExecution,
 }

+/// How the deployment's data plane runs work, for latency and the
+/// precompute load. Loadable from JSON; an omitted field takes its default.
+pub struct DataPlaneExecution {
+    /// Threads inserting arriving rows; `None`: one per core the ingestion
+    /// work needs (today's rule).
+    pub ingest_threads: Option<u32>,
+    /// Threads running window-close refreshes; default 1.
+    pub precompute_threads: u32,
+    /// Threads running query requests, one request per thread; default 1.
+    pub query_threads: u32,
+    /// Milliseconds the data plane waits after a window's end before
+    /// closing it; default 0.
+    pub allowed_lateness_ms: u64,
+    /// The largest precompute load (fraction of the precompute threads'
+    /// time spent refreshing) a valid plan may have, and the target
+    /// utilisation of the recommended thread counts; default 0.7.
+    pub max_thread_utilization: f64,
+}
```

`CostInputs::validate` refuses thread counts of 0 and a
`max_thread_utilization` outside (0, 1].

### 3.2 Measured costs (`asap-cost`, `measured.rs`)

```diff
 pub struct OperatorCost {
     ...
     pub ns_per_invocation: f64,
+    /// CPU nanoseconds per window close of an ingestion-time node, whatever
+    /// its units: the fixed cost of one refresh. Default 0.
+    #[serde(default)]
+    pub ns_per_refresh: f64,
     ...
 }
```

### 3.3 Latency and report (`asap-cost` `plan/cost.rs`, planner `installable`)

```diff
-pub struct QueryLatency {
-    pub compaction_ms: f64,
-    pub query_ms: f64,
-}
+/// One query's latency per request: the freshness wait, then the
+/// request's execution (section 2.1).
+pub struct QueryLatency {
+    /// The data plane's allowed lateness.
+    pub allowed_lateness_ms: f64,
+    /// Queueing behind the refreshes of other groups closing at the same
+    /// instant (the largest among the groups it reads).
+    pub precompute_queueing_ms: f64,
+    /// The refresh time of the group it waits for, partial-copy
+    /// compaction included.
+    pub precompute_execution_ms: f64,
+    /// The request's query-time work, its fixed overhead included.
+    pub query_execution_ms: f64,
+    /// Evaluations per second, its weight in the mean.
+    pub evaluations_per_second: f64,
+}
+impl QueryLatency {
+    /// The sum of the four parts.
+    pub fn latency_ms(&self) -> f64;
+}

 pub struct ResourceUsage {
     ...
+    /// Fraction of the precompute threads' time spent refreshing (ρ).
+    pub precompute_load: f64,
+    /// Per thread role, the predicted CPU and recommended thread count.
+    pub recommended_configuration: RecommendedThreads,
 }

+/// The threads a deployment needs to serve the plan (section 2.6).
+pub struct RecommendedThreads {
+    pub ingest: RoleThreads,
+    pub precompute: RoleThreads,
+    pub query: RoleThreads,
+}
+pub struct RoleThreads {
+    /// CPU core-seconds per second the role is predicted to use.
+    pub predicted_cpu_core_seconds_per_second: f64,
+    /// ceil(predicted cores / max_thread_utilization), at least 1.
+    pub recommended_threads: u32,
+}
```

`PredictedQueryUsage` (the plan report) takes the same five fields plus
`latency_ms`; `PredictedNodeUsage` gains `ingest_cpu_core_seconds_per_second`,
`refresh_ms_per_window` and `query_ms_per_run` for the calibration
comparison; `PredictedUsage` gains `precompute_load` and
`recommended_configuration`. The report's JSON
changes, so `INSTALLABLE_PLAN_WIRE_VERSION` goes from 15 to 16.

### 3.4 Stage 3 (`stage3-plan-selection`)

```diff
 pub struct Selection {
     ...
+    /// Under cost model 3's `LowestMeanLatency`, the selected plan's
+    /// frequency-weighted mean latency in ms; `None` otherwise.
+    pub selected_plan_mean_latency_ms: Option<f64>,
 }

 pub enum ErrorSelectingPlan {
     ...
+    /// Under cost model 3, no plan fits the cluster's limits; each listed
+    /// limit alone blocks every plan meeting the rest (section 2.7).
+    #[error("no plan fits the cluster's limits: {0:?}")]
+    OverClusterLimits(Vec<ClusterLimitShortfall>),
 }

+/// One cluster limit that alone blocks every plan, with the least any plan
+/// meeting the other limits needs.
+pub struct ClusterLimitShortfall {
+    pub limit: ClusterLimit,
+    pub least_needed: f64,
+    pub available: f64,
+}
+pub enum ClusterLimit {
+    MemoryBytes,
+    DiskBytes,
+    CpuCores,
+    PrecomputeLoad,
+    QueryLatencyMs { query: usize },
+}
```

The MILP rows live in a new `milp/latency.rs` (variables $F_q$, $L_q$, the
reads $a_{q,e}$ beyond (X), refresh-group expressions, latency rows) and
`milp/cluster_limits.rs` (memory, disk, CPU, precompute-load rows), next to
the existing families; `solve_selection` picks the objective and the
tie-break solves from `CostModel`.

### 3.5 Order of work

1. Tests of section 6, failing.
2. Calibration: `ns_per_refresh`, the ingest/refresh split per node, the
   per-node phase times in the report; the comparison procedure of section
   2.4 run against a deployment, with the measured table fed back.
3. Latency model in `plan_cost`: `DataPlaneExecution`, the new
   `QueryLatency`, $\rho$; report fields and wire version.
4. Stage 3: cost model 3 with `LowestMeanLatency`, the limit rows,
   tie-break solves, `OverClusterLimits`.

## 4. Decisions

| # | Decision | Status |
| --- | --- | --- |
| 1 | New `ClusterObjective::LowestMeanLatency`: minimize $\sum w_q L_q / \sum w_q$, $w_q$ = evaluations per second (fixed interval $1000/I$; a range evaluation counts its steps; non-repeating spread over the horizon); ties by peak memory, then disk. `LowestLatency` unchanged. `max_latency_ms` is a hard per-query bound. | Decided |
| 2 | $L_q$ = freshness wait (allowed lateness + precompute queueing + refresh execution of the groups $q$ reads) + query execution (critical path of $q$'s query-time sub-DAG, one run on one thread; a shared `OneEvaluation` node counted on every reader's path). | Decided |
| 3 | Thread counts and allowed lateness are deployment parameters in `CostInputs::data_plane_execution` (section 1.3 gives the reasons). | Decided by this document (user asked to choose and justify) |
| 4 | CPU limit = average cores used; memory peak $\le$ `memory_bytes`; disk $\le$ `disk_bytes`. | Decided |
| 5 | Calibrate before implementing the objective; feed results through the measured table, data facts and `frozen_calibration_factor`; generic procedure. | Decided |
| 6a | Precompute load $\rho = \sum_G (P_G/W_G) / \text{precompute\_threads}$; hard constraint $\rho \le \rho_{\max}$, default 0.7; plans above it are invalid. | Decided |
| 6b | Queueing time within $\rho \le \rho_{\max}$: the linear upper bound (coinciding groups' refresh times ÷ precompute threads). | Decided |
| 7 | MILP: per-query latency variables, max over freshness sources by big-M with computed bounds, objective $\sum w_q L_q$, resource rows; lexicographic second solves for ties; scaling as today; sharing through $a_{q,e}$. | Decided (formulation); tie-break choice by this document |
| 8 | On one thread the critical path is the sum of the sub-DAG's node times (each once), so query execution is a sum over $a_{q,e}$; the max is needed only over the freshness sources. | Proposed for v1 (open question 1) |
| 9 | Refresh group = ingestion-time nodes of one `RefreshWindow`, refreshed as one task on one precompute thread; compaction of partial copies moves from query latency into refresh time. | By this document |
| 10 | New `OperatorCost::ns_per_refresh` for the fixed cost per refresh. | By this document |
| 11 | Cost model 1 does not enforce latency bounds or $\rho \le \rho_{\max}$; cost model 1 plans do not change. | Decided |
| 12 | This work implements `LowestMeanLatency` only; `LowestLatency`, `LeastMemory` and cost model 2 keep `UnsupportedCostModel`. | By this document |
| 13 | Infeasible under cost model 3: per-limit relaxed solves on the error path, `OverClusterLimits` with least needed and available. | By this document |
| 14 | Under cost model 3 the greedy cost upper bound is not applied; the `OneEvaluation` twin dominance is restated over time, memory, disk and CPU. | By this document |
| 15 | Calibration passes when every node's predicted time per phase is within 1.5× of measured. | Decided |
| 16 | Ingest and query CPU are not hard constraints against thread counts; the report gives per role (ingest, precompute, query) the predicted CPU core-seconds per second and the recommended threads, $\lceil$cores$/\rho_{\max}\rceil$; `precompute_threads` stays an input for $\rho$, and a deployment may set it from the recommendation and replan. | Decided |
| 17 | One utilisation parameter, `max_thread_utilization`, is both $\rho_{\max}$ and the recommendation target. | By this document |

## 5. Open questions

1. **Critical path on one thread (decision 8, proposed for v1).** Section
   2.1 reads "one run per query on one thread" as: every node of the
   sub-DAG is on the critical path, so execution is the sum. If the data
   plane later runs independent branches of one request concurrently,
   replace the sum with per-node finish times
   $f_{q,e} \ge f_{q,u} + d_e - M(1 - y_{u \to e})$.
2. **Query queueing.** Requests waiting for a free query thread are not
   modelled; a deployment running fewer query threads than recommended
   sees latency above the prediction.

## 6. Test plan (for approval)

Each acceptance test names the behaviour, its inputs, the expected result,
and whether it fails on today's code. Workloads are PromQL over a gauge `m`
(1,000 series, 1 sample/s) unless stated; illustrative times are fixed
through a `MeasuredOperationCosts` table so the expected plan does not hang
on formula constants.

### 6.1 Acceptance

| # | Behaviour | Inputs | Expected | Fails today |
| --- | --- | --- | --- | --- |
| A1 | `LowestMeanLatency` picks a lower-latency plan than cost model 1's cheapest | the example (`max_over_time(m[5m])` every 10 s), a table pricing the chain refresh at 44 ms per window and the raw scan at 30 ms per run, cluster limits that fit both | cost model 1 selects the chain; `LowestMeanLatency` selects the query-time scan; mean latency 30 ms | yes (`UnsupportedCostModel`) |
| A2 | A plan over $\rho_{\max}$ is rejected | a workload whose lowest-latency plan has $\rho = 0.9$ with 1 precompute thread, $\rho_{\max} = 0.7$ | the next plan with $\rho \le 0.7$ is selected; the report's `precompute_load` $\le 0.7$ | yes |
| A3 | A tighter memory limit switches to a smaller-state plan | a quantile query whose lowest-latency plan keeps a large ingestion-time sketch; `memory_bytes` set first above, then below its peak | above: that plan; below: a plan with smaller peak memory, within the limit | yes |
| A4 | `max_latency_ms` is enforced | the A1 workload with a second query that reads a heavy refresh group; its bound set below that plan's latency | that query is answered by a plan within its bound, even though the mean rises | yes |
| A5 | Frequency weighting | two queries, one every 10 s, one every 5 min, where one plan favours each | the plan favouring the 10 s query; reversing the intervals reverses the choice | yes |
| A6 | Freshness counts | a query reading an ingestion-time view in a group with a heavy refresh (ns_per_refresh large) | its predicted latency includes that refresh; raising the refresh time switches it to a query-time plan | yes |
| A7 | Thread count changes the choice | the A2 workload with `precompute_threads` 1, then 2 | 1: the plan with $\rho \le 0.7$; 2: the lowest-latency plan ($\rho = 0.45$), queueing halved | yes |
| A8 | Ties by memory, then disk | two plans with equal mean latency and different peak memory; then equal latency and memory, different disk | the smaller peak memory; then the smaller disk; `milp_solve_count` 3 | yes |
| A9 | Report breakdown | the A1 and A6 workloads | per query, lateness + queueing + precompute execution + query execution = `latency_ms`; weights as section 1.1; the MILP's mean equals `plan_cost`'s mean of the selected plan | yes |
| A10 | Infeasible limits error | `memory_bytes` below every plan's peak; separately, a latency bound no plan meets | `OverClusterLimits` naming memory with the least needed and the limit; then the query's latency bound | yes |
| A11 | CPU limit is average cores | a plan whose average CPU core-seconds per second is 1.6 with `cpu_cores` 1, another at 0.8 | the 0.8 plan; with `cpu_cores` 2, the lower-latency one | yes |
| A12 | Range evaluation weight counts steps | a 1 h range request at 1 min steps (61 steps) every 5 min vs an instant query every 1 min | weights 61/300 and 1/60 per second; the choice follows them | yes |
| A13 | Non-repeating weight | a one-off query with 10 invocations, horizon 3,600 s | weight 10/3,600 per second | yes |
| A14 | Allowed lateness | the A1 workload with `allowed_lateness_ms` 5,000 | both plans' latency rises by 5,000 ms; the choice is unchanged | yes |
| A15 | Sharing counts on every reader | two queries reading one shared `OneEvaluation` node | its time is in both queries' `query_execution_ms`, its memory once | yes |
| A16 | Recommended configuration | a plan with 0.9 refresh, 0.3 ingest and 1.5 query CPU core-seconds per second, $\rho_{\max} = 0.7$ | recommended threads: precompute 2, ingest 1, query 3; each role's predicted cores reported; replanning with `precompute_threads` = 2 returns 2 again | yes |
| A17 | Ingest and query CPU are not limited by thread counts | a plan whose query CPU exceeds `query_threads` | still selected (no rejection); recommended query threads above `query_threads` | yes |

### 6.2 Calibration

| # | Behaviour | Inputs | Expected | Fails today |
| --- | --- | --- | --- | --- |
| C1 | `ns_per_refresh` is charged once per window close | an ingestion-time chain of 1 min windows with `ns_per_refresh` 44e6 | its refresh time per window rises by 44 ms, its CPU by 44 ms ÷ 60 s, $\rho$ accordingly | yes (no field) |
| C2 | A query-time fixed cost per run is charged per run | `max_over_time` with `ns_per_invocation` 5e6 at query time | its `query_ms_per_run` rises by 5 ms | no (guard) |
| C3 | A declared value range replaces the DDSketch default | a DDSketch merge with `column_magnitudes` [1, 1e3] | smaller merge time than with the default; no `AssumedDataFact` | no (guard) |
| C4 | Per-node phase times are reported | any plan with ingestion-time and query-time nodes | each node's `ingest_cpu_core_seconds_per_second`, `refresh_ms_per_window`, `query_ms_per_run` | yes |

### 6.3 Regression

| # | Behaviour | Inputs | Expected | Fails today |
| --- | --- | --- | --- | --- |
| R1 | Cost model 1 plans unchanged | the sketch plan snapshots and the Stage 3 objective tests, also workloads with `max_latency_ms` | identical plans and costs; no latency bound or $\rho_{\max}$ enforced | no (guard) |
| R2 | Other objectives unchanged | cost model 2; cost model 3 with `LowestLatency` and `LeastMemory` | `UnsupportedCostModel` as today | no (guard) |
| R3 | Old cost-inputs JSON loads | a `CostInputs` file without `data_plane_execution` or `ns_per_refresh` | defaults of section 1.3; validation refuses 0 threads and a load bound outside (0, 1] | no for loading (guard), yes for the new validation |

### 6.4 Unit tests of the rows

- Coinciding groups: $(60\text{ s}, 0)$ and $(300\text{ s}, 0)$ coincide;
  $(60\text{ s}, 0)$ and $(60\text{ s}, 30\text{ s})$ do not.
- $M_G$ is an upper bound of $f_G$ for every selection of a small catalog
  (enumerated).
- The reads $a_{q,e}$ of the optimum equal the sub-DAG reachable from
  $q$'s output in the selected plan.
