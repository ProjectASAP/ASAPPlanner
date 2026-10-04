# Stage 3 Cost Model

> Status: partly implemented. Audience: planner designers and architects.
> Scope: the cost that #509 Stage 3 (plan selection) minimizes.
> Companion: [ASAPPlanner layering](planner-layering.md), Stage 3.

## Goal

Stage 3 selects the cheapest valid physical candidate for the whole workload.
Candidates differ in what they compute, and also in when they compute it: at
ingestion time, continuously, or at query time, on each evaluation. To compare
them, every candidate is priced in one unit, **cost per second of wall time**,
for the workload in steady state.

## What Stage 3 prices

| Priced | Not priced (yet) |
|---|---|
| CPU work of every operator, at ingestion time or at query time | Transient query-time memory |
| Bytes a scan reads | Storage tier and retention: disk versus memory, and for how long (S3, with panes) |
| Memory held across evaluations by ingestion-time state that query time reads | Network, parallelism and partitioning |
| | Latency bounds (a separate check, below) and deployment capabilities |
| | One-time setup work such as backfill (deferred, S5) |

Accuracy and latency are checks, not costs. Stage 3 rejects candidates that
miss a target, then prices the rest.

## Time basis

The price of a node depends on when the node runs.

**Ingestion-time node.** The node runs as data arrives. It is priced over one
second of ingested rows:

```text
cost_ingest(n) = c_cpu · ops(n, λ rows) + c_scan · scan_bytes(n, λ rows) + memory(n)
```

λ is the ingestion rate in rows per second. It is
`DataWorkload.ingestion_rate` if declared, else `input_cardinality /
data_ingestion_interval` (one sample per series per interval), else the
default in the statistics table below.

**Query-time node.** The node runs on each evaluation of a query that reads it.
It is priced per evaluation, times its evaluation rate `r(n)`:

```text
cost_query(n) = r(n) · (c_cpu · ops(n, one evaluation) + c_scan · scan_bytes(n, one evaluation))
```

A query-time scan reads as far back as the time ranges reading it reach:
the longest range plus its offset. Each range then passes only its own
span. For example, `x[1y] offset 2y` scans 3 years, and a 1-year range over a
shared 5-year scan passes 1 year of rows.

**Workload cost** is the sum over the candidate's DAG nodes:
`cost(P) = Σ_n cost(n)`, in cost per second.

## Recurrence and horizon

`r(n)` comes from the recurrence of the roots (queries) whose DAG reaches `n`:

* **Repeating roots** with interval `t` contribute `1 / t` evaluations per
  second. Roots with **equal intervals count once**, because those queries are
  evaluated together and read one result. Distinct intervals add:
  `r = Σ_{distinct t} 1 / t`.
* **Estimated-rate roots** contribute their expected rate.
* **One-off, scheduled and unknown roots** run as one batch. Their work is
  amortized over a horizon `H`: the most invocations among them, divided by
  `H`. `Unknown` counts as one invocation. `H` is 1 h by default (S5).

So a one-off query costs `invocations · per-evaluation cost / H`. That keeps a
one-off query comparable with a repeating one, and puts ingestion-time
maintenance for a one-off query at its true disadvantage: it runs every second
for an answer that is needed once per hour.

## Memory term

State that is retained across evaluations costs memory for as long as it is
kept:

```text
memory(n) = w_mem · retained_bytes(n)
w_mem     = 1.25e-7 cost / (byte · s)
```

**Derivation.** One cost unit is one CPU-millisecond. 1 GB of memory is
priced like 1/8 of a vCPU, the memory-to-core ratio of memory-optimized cloud
instances (8 GB per vCPU): 125 CPU-ms per second for 10⁹ bytes, or 1.25e-7
per byte per second.

**What counts as retained.** Only ingestion-time state that a query-time node
reads: it lives between evaluations. Retained bytes are

```text
retained_bytes(n) = 2 · groups(n) · state_bytes(n)
```

where `state_bytes` is the summary's state size (for example
`8 · width · depth` for Count-Min) or the row size of an exact state. The
factor 2 covers the window being built plus the completed window that
queries read. Query-time state, built and discarded within one evaluation, is
transient and not priced.

**Tumbling panes at ingestion time.** A window of `lookback` read every
evaluation as `N = lookback / w` panes of width `w` is, at ingestion time,
one pane built as rows arrive and kept for the next evaluations: pane `i` of
this evaluation is the newest pane of the evaluation `i · w` earlier. So the
newest pane (the smallest shift) pays the build, over λ rows per second, and
the memory of itself and the `N` completed panes:

```text
retained_bytes(newest) = (N + 1) · groups · state_bytes
```

The older panes, and the shifts and ranges that feed only them, cost nothing.
When panes are shared by windows of different lengths, the longest sets `N`.
The merge and the estimate run at query time, per evaluation. Retention is
in memory only (S3).

## Latency check

A query's `response_latency` bound (S6) is checked against the query-time
work it waits for in one evaluation: the per-evaluation cost of every
query-time node the query reaches, shared nodes included, times
`latency_ms_per_cost_unit`. Ingestion-time work is done before the query
asks, so maintaining state at ingestion time moves work out of the bound. A
candidate over the bound is rejected with the query, the estimate and the
bound, for example `q1: query-time work takes 310.0 ms per evaluation, over
the 200 ms latency bound`. The estimate assumes one core and no queueing.

## Calibration

Every coefficient lives in `Stage3Calibration`, carried by `PlanningModels`
and set with `PlanningModels::with_calibration`:

| Field | Default (`ILLUSTRATIVE`, `illustrative-v2`) | Meaning |
|---|---|---|
| `cost_per_cpu_op` | 1e-6 | 1 ns of CPU per operation, in CPU-ms |
| `cost_per_scan_byte` | 1e-7 | per byte read by a scan |
| `cost_per_retained_byte_second` | 1.25e-7 | `w_mem` |
| `horizon_s` | 3600 | `H` |
| `latency_ms_per_cost_unit` | 1 | ms of response time per cost unit of query-time work: one CPU-ms on one core (1e6 operations or 1e7 scanned bytes per ms) |
| `version` | `illustrative-v2` | reported in each candidate's cost `source` |

A deployment that prices memory differently, for example memory-rich
instances, lowers `cost_per_retained_byte_second`; one that plans for a daily
batch raises `horizon_s` to 86 400.

## Default statistics

When the workload does not declare a statistic, Stage 3 uses these defaults.
They are **illustrative**: they order candidates plausibly, but they are not
measured and the absolute costs mean little.

| Statistic | Default |
|---|---|
| Series (`input_cardinality`) | 1 000 |
| λ, rows per second | 1 000 |
| Lookback of a scan read by no time range | 1 min |
| Groups of a `by (...)` reduction | 100 |
| Summary update operations per row | sketch depth (+1 with a heap) |
| Summary state bytes | `8 · width · depth` (+24 per heap entry); 1 KiB for other sketches |
| Row bytes | 8 per plain value, 16 per string |

## Interaction with selection

**Additivity.** Stage 3's dynamic program over target nesting
([#572](https://github.com/ProjectASAP/ASAPPlanner/issues/572)) needs cost to
be a sum over nodes, with a choice for one target changing only its own nodes.
The per-second cost keeps this: each node's price depends on its own
statistics, its timing, and the set of roots that reach it. The roots that
reach a node do not depend on how other targets are realized. The program
still verifies additivity for every nested pair and falls back to enumeration
when it does not hold.

**Shared nodes are charged once.** One DAG node is one computation. A node
reached by several queries is priced once, at the evaluation rate of the
distinct intervals of those queries. That is, a shared node is materialized
at query time within the batch: computed once per evaluation and read by every
consumer.

**Decision: "not materialized" is a DAG shape, not a cost rule.** #509 Stage 2
lists "not materialized" as an option for a multi-consumer sub-DAG (Example 4,
A3): each consumer recomputes it. Stage 2 represents that option by duplicating
the node for each consuming query, so the duplicates are priced separately. The
cost model never discounts or multiplies a node by its number of consumers.

## Worked example: Example 1

Example 1 has two panels, each repeating every 10 s, over 1 000 000 series
ingested every 15 s (λ = 66 667 rows/s). In the selected plan every node runs
at query time (no summary in it can be maintained: its windows slide). Both
roots have the same 10-s interval, so every node, shared or not, has
`r = 0.1`/s.

P60, the selected plan (Q1 exact; Q2 Count-Min + heap over an exact
`sum_over_time` accumulator, valid because `http_requests_total` is declared a
counter; the input shared by both queries):

| Node | Reached by | Work per evaluation | Per evaluation | r (/s) | Per second |
|---|---|---|---|---|---|
| Scan | Q1, Q2 | 4 000 000 samples (1 min × λ) | 23.2 | 0.1 | 2.32 |
| Time range 1 min | Q1, Q2 | 4 000 000 rows | 4.0 | 0.1 | 0.40 |
| Rate accumulator | Q1 | 4 000 000 rows into 1 000 000 states | 4.0 | 0.1 | 0.40 |
| Finalize | Q1 | 1 000 000 accumulators | 1.0 | 0.1 | 0.10 |
| Sum by `job` accumulator | Q1 | 1 000 000 rows into 100 states | 1.0 | 0.1 | 0.10 |
| Finalize | Q1 | 100 accumulators | 0.0001 | 0.1 | 0.00001 |
| Sum accumulator (`sum_over_time`) | Q2 | 4 000 000 rows into 1 000 000 states | 4.0 | 0.1 | 0.40 |
| Finalize | Q2 | 1 000 000 accumulators | 1.0 | 0.1 | 0.10 |
| Count-Min + heap by `job` | Q2 | 1 000 000 rows × depth 8 into 100 states | 8.0 | 0.1 | 0.80 |
| Top-10 estimate | Q2 | 1 000 rows from 100 states | 0.001 | 0.1 | 0.0001 |
| **Total** | | | **46.201** | | **4.620** |

The runner-up is P44 at 4.720 per second; the same choices with separate
inputs (P28) cost 7.340, because the scan and range are charged twice.

Before per-second pricing, P60 cost 46.201 CPU-ms per workload evaluation. Now
it costs 46.201 × 0.1 = 4.620 per second. Every other all-query-time
candidate scales by the same factor, so their ranking is unchanged.

**With materialization.** Stage 2 also offers Q2's exact `sum_over_time` in
six 10-s panes maintained at ingestion time. The cheapest such candidate costs
47.407 per second: the panes' ingestion work is small (scan 0.387, shift and
range 0.133, newest-pane build 0.067), but the newest pane retains seven
panes of 1 000 000 per-series sums, 336 MB, for 42.0 per second of memory.
The selection is unchanged. Q2's 100 ms latency bound rejects 40 candidates,
every Count-Sketch + heap among them.

## Out of scope for now

* **Not materialized for several consumers (Q44, Example 4 A3).** Stage 2
  would duplicate the sub-DAG per consuming query; it does not generate that
  option yet.
* **Query time, kept (Example 4 B3).** A pane built at query time and kept
  for later evaluations is not offered; panes are rebuilt at query time or
  maintained at ingestion time.
* **Storage tier and retention (S3).** Retained state is priced as memory.
  Choosing disk versus memory and how long to keep state comes with panes.
* **Calibration from measurements.** The coefficients and default statistics
  are illustrative. Fitting them to measured operator costs, and pricing with
  observed statistics such as group counts, is future work.
* **Backfill.** Building ingestion-time state over existing data when a query
  is installed is a one-time cost and is not priced (S5).
