# Stage 3: Precompute Queueing Rows That Keep the Lower Bound

**Status:** Proposal, for review before any code. A prototype of the rows
(not committed) gave the measurements below.

**Audience:** Planner designers and implementers of plan selection (Stage 3)
under cost model 3's `lowest_mean_latency`
(`stage3-lowest-mean-latency.md`).

## Example

The dashboard at 10,000 series
(`tools/dag-viewer/workloads/typical-dashboard-10k/workload.json`, measured
cost inputs `cluster-lowest-mean-latency-measured.json`, deployment
`mvp`: raw data not kept): 11 queries, among them
`sum by (job) (rate(data_total[5m]))` and
`avg by (region) (quantile_over_time(0.5, data[1h]))`, every 15 s, with
two precompute threads and allowed lateness 0.

Its ingestion-time executions fall into 8 refresh groups: the tumbling
window chains at 5, 10, 15, 20 and 30 s and the wider levels at 1, 5 and
15 min, all with phase 0, so every two groups close together at some
instant. The selected plan's mean latency is about 837 ms, of which about
277 ms is waiting for precompute (queueing behind other groups' refresh,
113–207 ms per query, and the own group's refresh, 37–225 ms) and the rest
query-time work.

The MILP's LP relaxation drops that wait:

| | Root LP mean latency (freshness + query work) | Default stop rule (1 % gap or 10,000 nodes) |
| --- | --- | --- |
| without levels, today | 321 ms (22 + 299) | 2.7 s, 848.55 ms, gap 0.19 % |
| with levels (levels admitted under this objective), today | 125 ms (27 + 98) | gap 26.8 % after 600 s (2,330 nodes); with a 200-node limit: 132 s, 848.55 ms, gap 44.8 % |
| without levels, with the rows below | 367 ms (67.5 + 299) | 2.3 s, 850.08 ms, gap 0.81 % |
| with levels, with the rows below | 238 ms (106 + 132) | **193 s, 838.08 ms, gap 0.95 %** |

With the rows, the selection with levels reaches the 1 % gap. Without
them, it does not reach it in 600 s.

## 1. Components

### 1.1 Today's freshness rows

Per refresh group $G$ (executions with one `RefreshWindow`), its refresh
time per window is linear in the selection:

$$P_G = \sum_{e \in G} p_e\,x_e + \sum_{u \to e,\ e \in G} s_{u \to e}\,y_{u \to e} + \text{wait}\cdot\mathbb{1}[G \text{ selected}]$$

and its freshness, the wait of a query reading it, is

$$f_G = a + \sum_H c(G, H)\,P_H, \qquad c(G, H) = \begin{cases} 1 & H = G \text{ or } G \text{ reads } H \\ 1/\text{threads} & H, G \text{ close together} \\ 0 & \text{otherwise.} \end{cases}$$

A query $q$ waits for the freshness of every group it reads:
$F_q \ge f_G - M_G\,(1 - r_{q,e})$ for each ingestion-time execution
$e \in G$ it may read, with $r_{q,e} \in [0, 1]$ its read of $e$ and
$M_G$ the largest $f_G$ can be. The rows without the big-M,
$F_q \ge a + (p_e + \text{wait})\,r_{q,e}$ and
$F_q \ge a + \sum_{e \in G} p_e\,r_{q,e}$, charge only the read
executions' own refresh.

In the LP relaxation $r_{q,e}$ is fractional, so the big-M rows vanish, and
what is left charges a query nothing for the other groups' refresh. That
is the queueing: about 113–207 ms of each query's 277 ms wait.

### 1.2 The query's possible refresh groups

For query $q$:

- $R(q)$: the refresh groups of the ingestion-time executions it may read
  (its reach, stopping at ingestion time).
- $\text{must\_read}(q)$: whether every plan reads an ingestion-time
  execution for $q$, so that no answer of $q$ can be computed at query time
  from executions all at query time. It holds for every query of the example:
  the deployment keeps no raw data, so every read starts at an ingestion-time
  execution.

### 1.3 The queueing row

For a query with $\text{must\_read}(q)$ and $R(q)$ not empty:

$$F_q \ge a + \sum_H \Big(\min_{G \in R(q)} c(G, H)\Big)\,P_H \qquad \text{(Q)}$$

It is linear in $x$, $y$ and the groups' selection indicators, with no read
variable. A query reading one group pays that group's whole refresh, the
whole refresh of the groups that group reads, and $1/\text{threads}$ of
every other group closing with it, whichever plan is selected.

Example: `sum by (job) (rate(data_total[5m]))` may read only its counter
chain's group $G_c$ (15 s), which closes together with all 7 others and reads
none. Its row is

$$F_q \ge 0 + P_{G_c} + \tfrac12 \sum_{H \ne G_c} P_H.$$

A query that may read the 1 min level or its 15 s chain has $R(q)$ = {both
groups}. The 1 min group reads the 15 s one, so both take coefficient 1;
every other group takes $1/2$.

## 2. Algorithm

1. Before the MILP, per query: $R(q)$ from the reach and the refresh
   groups; $\text{must\_read}(q)$ by a fixpoint over the kept executions
   and selectable edges. An execution can be answered at query time only
   if it runs at query time and each of its inputs has such a producer. The
   query must read if none of its answering executions can.
2. Per query with $\text{must\_read}(q)$ and $R(q) \ne \emptyset$: per group
   $H$, its coefficient $\min_{G \in R(q)} c(G, H)$, and the row (Q) over
   the groups' $P_H$ expressions already built for $f_G$. There is one row
   per query, so the model gains 11 rows on the example and no variable.
3. Everything else is unchanged: the big-M rows, the per-execution rows, the
   reads along each path, and pinning the selected plan for its latencies.

**Validity: every optimum is kept.** Take any plan and query $q$. By
$\text{must\_read}(q)$ the plan reads some ingestion-time execution
$e \in G^\ast$ for $q$, with $G^\ast \in R(q)$, and the model's latency gives
$F_q \ge f_{G^\ast} = a + \sum_H c(G^\ast, H) P_H$. Every
$P_H \ge 0$ in a plan: per selected consumer $e$, $p_e$ plus its selected
edges' shares is its refresh with the producers it reads, which is at least
0, and the wait term is at least 0. So
$\sum_H c(G^\ast, H) P_H \ge \sum_H \min_{G \in R(q)} c(G, H) P_H$. Every
plan meets (Q) at its own latencies, so the feasible plans and their
objective values are unchanged. A test checks $P_H \ge 0$ (section 5).

**Why the bound rises.** In the LP every query must still be answered: its
reads flow down to ingestion-time executions whose $x$ are at least those
reads. The total refresh selected for all queries then lower-bounds every
query's queueing, through $1/\text{threads}$ of it, with no product of a read
and a selection. On the example the root LP's freshness goes from 27 to
106 ms with levels, and from 22 to 67.5 ms without.

**What it leaves.** The root LP is still 238 ms against about 837 ms. The
wait for one's own group's refresh beyond $1/\text{threads}$ still needs the
read (a product), and the LP still splits query-time work over alternatives.
The default stop rule nevertheless closes the gap within its nodes on the
example.

**Considered and measured, not proposed.**

| Rows | Root LP with levels | Default stop rule with levels |
| --- | --- | --- |
| a read of an ingestion-time execution waits for its producer's refresh (a flow over its input edges) | 137 ms | gap 27.1 % after 600 s |
| the big-M row per term of $f_G$, with a group read $g \ge r_{q,e}$ | 125 ms (unchanged) | gap 25.2 % after 319 s; without levels, over 336 s instead of 2.7 s |
| $L_q \ge (\ell^{\min}_c + \text{request}_q)\,z_{q,c}$ per answer | 125 ms | gap 14.9 % after 600 s; without levels, 7.0 s instead of 2.7 s |

The rows above multiply a query's read by the selection of another query's
executions, and the LP sets that product near 0. The queueing row (Q) has
no such product.

**Complexity.** One row per query, with up to one term per refresh group:
$O(|Q| \cdot |\text{groups}|)$ nonzeros (88 on the example). $R(q)$ and
$\text{must\_read}(q)$ take one pass over each query's reach and edges.

## 3. Code interface

In `crates/stage3-plan-selection/src/milp/model/cluster/mod.rs`:

```rust
struct QueryShape {
    query: QueryId,
    weight: f64,
    request_ms: f64,
    step_rounds: u64,
    request_cpu_ms: f64,
    max_latency_ms: Option<f64>,
    answering: Vec<usize>,
    reach: BTreeSet<usize>,
    edges: Vec<usize>,
    /// The refresh groups of the ingestion-time executions it may read
    /// (R(q)), by position in `Groups::windows`.
    refresh_groups_it_may_read: BTreeSet<usize>,
    /// Whether every plan reads an ingestion-time execution for it: none of
    /// its answers can be computed from query-time executions alone.
    reads_at_ingestion_time_in_every_plan: bool,
}
```

`Groups` is unchanged:

```rust
struct Groups {
    windows: Vec<RefreshWindow>,
    executions: Vec<Vec<usize>>,
    of_execution: HashMap<usize, usize>,
    reads: Vec<BTreeSet<usize>>,
}
```

In `crates/stage3-plan-selection/src/milp/model/cluster/solve.rs`,
`build_and_solve` adds row (Q) per query, after the refresh expressions
$P_H$ (`refresh`) and before the latency rows, and logs its rows as their
own family (`queueing`) when the solver log is on.

In `crates/cost/src/model.rs`, a switch beside the other speed-ups, to
check that the rows keep the optimum:

```rust
pub struct LatencySelectionChecks {
    pub theta_dominance: bool,
    pub feasible_plan_bound: bool,
    pub lazy_tie_breaks: bool,
    pub path_reads: bool,
    pub report_root_lp_bound: bool,
    pub log_solver: bool,
    pub node_limit: i32,
    /// Add the queueing row per query (section 2); default true.
    pub queueing_rows: bool,
}
```

The planner binary reads `ASAP_STAGE3_QUEUEING_ROWS=off`.

## 4. Decisions and open questions

- **Open:** whether selecting with levels still needs its own node limit
  (the proposal for selecting levels under this objective stops after 200
  nodes, 132 s on the example) once the rows are in: with the rows, the
  default stop rule reaches 1 % on the example in 193 s.
- **Open:** a row for one's own group's refresh beyond $1/\text{threads}$
  without a read variable. The candidates found so far all multiply a read
  by a selection.

## 5. Test plan (failing tests first)

| # | Test | Checks | Fails today because |
| --- | --- | --- | --- |
| Q1 | `the_queueing_row_keeps_the_optimum` (devtools `lowest_mean_latency_exactness`) | exact solve (`ASAP_STAGE3_EXACT=1`) with and without the rows (`ASAP_STAGE3_QUEUEING_ROWS=off`): the same mean latency on the measured workloads and the snapshot workloads with levels | the switch does not exist |
| Q2 | `the_queueing_row_raises_the_root_bound` (devtools) | typical-dashboard-10k without levels: root LP mean latency (`ASAP_STAGE3_ROOT_LP=1`) at least 360 ms (321 ms today) | the row does not exist |
| Q3 | `refresh_of_a_group_is_never_negative` (stage3 unit) | for each selected consumer in a small catalog with negative edge shares, every selection's $P_H \ge 0$ (enumerated) | new guard of the validity argument |
| Q4 | `a_query_answered_at_query_time_gets_no_queueing_row` (stage3 unit) | a deployment keeping raw data, a query that can be answered from a query-time scan: $\text{must\_read}$ false, no row | new |
| Q5 | `typical_dashboard_with_levels_reaches_the_gap` (devtools, ignored: minutes) | with levels, the default stop rule ends at a gap of at most 1 % | today it stops at the node limit with a gap of 44.8 % |
