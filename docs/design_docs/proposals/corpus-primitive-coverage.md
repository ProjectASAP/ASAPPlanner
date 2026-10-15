# Corpus coverage of ASAP primitives and summaries

> Status: measurement and gap analysis at `main` commit `80fc72e1`. The
> numbers describe that commit; rerun the method below before relying on them
> after the frontends, Pass 1 or plan selection change.

## Purpose

The repository carries eighteen query corpora as frontend tests. Those tests
check that lowering is total and, for some corpora, how much of the corpus
lowers. They do not say which ASAP primitives the queries need, or how many
queries the planner can answer with a summary today.

This document answers three questions for developers and researchers deciding
what to build next:

1. Which aggregation intents, grouping shapes, windows and composition shapes
   the corpora use.
2. For each query, how far the planner pipeline gets: Stage 0 lowering, Stage
   1 Pass 1 candidates, composition, Stage 2 implementation and Stage 3
   validity.
3. Which missing primitives, summary families, Pass 2 rules and guarantees
   block the most corpus queries.

Terms follow [planner-layering.md](planner-layering.md) and
[glossary.md](../concepts/glossary.md).

## Corpora

| Corpus | Language | Source | Queries | What the test asserts today |
|---|---|---|---:|---|
| PromQL docs examples | PromQL | `promql_corpus_docs.txt`: PromQL basics docs and a cheat sheet | 49 | Lowering is total (no panic); at least 47 lower. Post-ASAP binding never errors at ε = 0.01. |
| PromQL engine testdata | PromQL | `promql_corpus_testdata.txt`: every `eval` expression of the Prometheus engine tests | 1,822 | Lowering is total; at least 1,121 lower. Binding never errors. |
| o11y-bench PromQL | PromQL | `o11y_bench_promql.txt` | 27 | All 27 lower; the count is pinned. |
| awesome-prometheus-alerts | PromQL | `awesome_prometheus_alerts.txt`: alert `expr` strings | 949 | Lowering is total; no parse failure; at least 800 lower; shape tests for common patterns. |
| Metrics-observability (9 fixtures) | PromQL | `observability/data/metrics_observability/`: query logs (A, B), dashboards (A, Grafana, Kubernetes-mixin), rules and alerts (A, Kubernetes-mixin alerts and rules), Promset; see [metrics-observability-corpora.md](../../develop_docs/metrics-observability-corpora.md) | 5,132 | Each fixture is non-empty and every query is classified exactly once; counts are printed, not pinned. One ignored test covers aggregates over a `topk` output. |
| SQL packet-trace fidelity | SQL (DataFusion) | `synthetic_packet_trace_queries.sql`: fidelity metrics for synthetic packet traces | 70 | All 70 lower; shape tests for grouped counts and tuple distinct counts. |
| SQL TPC-H data-quality checks | SQL (DataFusion) | `tpch_deequ_queries.sql`: completeness, uniqueness, validity and distribution checks over `lineitem` | 50 | All 50 lower. |
| SQL netflow | SQL (DataFusion) | `netflow.sql` | 7 | Each query lowers to a pinned aggregate shape. |
| SQL BGP analytics | SQL (ClickHouse) | `bgp_analytics.sql` | 15 | Per-query outcome is pinned: 5 lower, 8 hit unknown ClickHouse functions, 2 hit grammar gaps. |
| SQL BGP analyst workload | SQL (ClickHouse) | `bgp_jan2024_rrc00_200_query_workload.yaml` | 200 | Aggregate tally is pinned: 106 lower, 54 fail with an invalid representation, 40 fail DataFusion planning. |
| **Total** | | | **8,321** | |

"A" and "B" name the fixtures from one source by the order the
metrics-observability test lists them. The MetricsQL golden test
(`victoriametrics_go_golden.rs`, five queries) checks parser canonicalization
only and is excluded. Five single-query SQL fixtures under
`frontend-sql/tests/data/` belong to one lowering test, not to a corpus, and
are also excluded.

## Method

Every query of every corpus was run through the library pipeline, one query
per workload, with the same calls that `stage_pipeline` makes:

1. **Stage 0.** PromQL through `lower_promql_query_workload` (scalar roots
   kept) and the series-identity column; SQL through `lower_sql_dialect` with
   each corpus test's own catalog and dialect. Query splitting follows each
   corpus test.
2. **Shape census.** A walk of the Stage 0 DAG records every `Aggregate`
   measure (`AggIntent` variant and reduction: global, `by(n)`, `without(n)`
   or per-entity), range selectors, subqueries, offsets, joins, binary
   operators between two operator inputs, `Filter`s, `Sort`/`Limit`, SQL
   window functions and the depth of nested aggregates.
3. **Stage 1.** `generate_per_computation_candidates` gives the targets and
   their candidates (from `summary_candidates` and the exact options), then
   every Stage 1 candidate (Pass 1 composition and the Pass 2
   identical-expression rule) is enumerated, up to 10⁴ candidates, the
   `stage_pipeline` default.
4. **Stages 2 and 3.** `run_stage2_and_stage3` with the built-in models. A
   candidate is valid when Stage 3 prices it (it meets the accuracy target).
   Each physical candidate is traced back to its Pass 1 choice, so a valid
   candidate is known to use a summary family or only exact options.
5. **Cross-query sharing.** For each target, three fingerprints of the Stage
   0 DAG: the aggregate and measure; the aggregate's input sub-DAG; and that
   input with every window length erased. Within a corpus they count queries
   that could use each Pass 2 rule (see [Pass 2 sharing
   potential](#pass-2-sharing-potential)).

Every query uses accuracy target ε = 0.01, δ = 0.001, a 15-second data
ingestion interval, unknown recurrence and no other data-workload evidence:
no metric types, no distinct-count bounds, no measured accuracy. Without an
approximate target Pass 1 offers no summary, so the target is required to see
summary candidates at all.

Each query falls in the first matching outcome:

| Outcome | Meaning |
|---|---|
| Parse: template | The parser rejects the text and it contains Grafana template syntax (`$var`, `[[var]]`, `label_values`). |
| Parse: other | Any other parse failure. |
| Lowering fails | Stage 0 rejects the query (PromQL lowering, series identity, or SQL planning). |
| No aggregate | The DAG has no `Aggregate`: selectors, arithmetic over selectors, threshold comparisons. |
| No ASAP candidate | It has aggregates, but no measure is a Pass 1 target (no exact accumulator, no summary). |
| Exact accumulators only | Every target offers only exact options (`ExactAccumulator`, `AvgAsSumCount` or `Exact`). |
| Summary offered, none valid | A summary family is offered, and Stage 3 rejects every candidate that uses one. |
| Summary valid: count total only | A summary candidate is valid, but only for `count` targets (see [Count summaries](#4-count-summaries-are-per-group-totals)). |
| Summary valid: approximate | A summary candidate is valid for a quantile, distinct-count or top-k target. |
| Other | Stage 3 failed (3 queries) or more than 10⁴ Stage 1 candidates (2 queries). |

### Reproducing

The corpus tests:

```sh
cargo nextest run -p asap-frontend-promql --test promql_corpus --test o11y_bench_promql \
  --test awesome_prometheus_alerts --test metrics_observability
cargo nextest run -p asap-frontend-sql --test synthetic_packet_trace --test tpch_deequ \
  --test netflow --test bgp_analytics --test bgp_jan2024_workload
```

One query through every stage, with the same target:

```sh
cargo run -p asap-devtools --bin stage_pipeline -- \
  --promql 'quantile_over_time(0.95, request_duration_seconds[1h])' \
  --epsilon 0.01 --delta 0.001 --out run.json
```

`sketch_coverage` reports Pass 1 summary candidates for seven of the corpora.
The sweep over all eighteen corpora used a local driver around the calls
above; it is not part of the repository, so the aggregate tables are
reproducible only by rebuilding that loop.

## Results

### Outcome per corpus

| Corpus | Queries | Parse: template | Parse: other | Lowering fails | No aggregate | No ASAP candidate | Exact accumulators only | Summary offered, none valid | Summary valid: count total only | Summary valid: approximate | Other |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| PromQL docs examples | 49 | 0 | 0 | 1 | 23 | 5 | 18 | 1 | 1 | 0 | 0 |
| PromQL engine testdata | 1,822 | 0 | 233 | 516 | 455 | 196 | 373 | 1 | 34 | 14 | 0 |
| o11y-bench PromQL | 27 | 0 | 0 | 0 | 1 | 0 | 26 | 0 | 0 | 0 | 0 |
| awesome-prometheus-alerts | 949 | 0 | 0 | 2 | 448 | 53 | 428 | 1 | 17 | 0 | 0 |
| MO logs A | 218 | 0 | 0 | 1 | 34 | 21 | 146 | 0 | 16 | 0 | 0 |
| MO logs B | 128 | 0 | 3 | 0 | 41 | 12 | 61 | 3 | 7 | 1 | 0 |
| MO dashboards A | 506 | 259 | 1 | 0 | 71 | 1 | 166 | 0 | 8 | 0 | 0 |
| MO Grafana dashboards | 522 | 168 | 3 | 2 | 185 | 0 | 147 | 0 | 17 | 0 | 0 |
| MO Kubernetes-mixin dashboards | 416 | 231 | 0 | 0 | 30 | 0 | 153 | 0 | 2 | 0 | 0 |
| MO rules A | 385 | 0 | 104 | 1 | 52 | 26 | 185 | 0 | 17 | 0 | 0 |
| MO Kubernetes-mixin alerts | 60 | 0 | 0 | 0 | 20 | 11 | 24 | 0 | 3 | 0 | 2 |
| MO Kubernetes-mixin rules | 71 | 0 | 0 | 3 | 0 | 2 | 66 | 0 | 0 | 0 | 0 |
| MO Promset | 2,826 | 65 | 386 | 30 | 719 | 95 | 1,420 | 13 | 81 | 15 | 2 |
| SQL packet-trace fidelity | 70 | 0 | 0 | 0 | 2 | 8 | 12 | 28 | 20 | 0 | 0 |
| SQL TPC-H data-quality checks | 50 | 0 | 0 | 0 | 0 | 2 | 32 | 8 | 7 | 0 | 1 |
| SQL netflow | 7 | 0 | 0 | 0 | 0 | 0 | 3 | 1 | 1 | 2 | 0 |
| SQL BGP analytics | 15 | 0 | 0 | 10 | 0 | 0 | 1 | 1 | 3 | 0 | 0 |
| SQL BGP analyst workload | 200 | 0 | 0 | 94 | 26 | 2 | 7 | 19 | 52 | 0 | 0 |
| **Total** | **8,321** | **723** | **730** | **660** | **2,107** | **434** | **3,268** | **76** | **286** | **32** | **5** |

Headline numbers:

- 6,208 of 8,321 queries (74.6%) lower. Of the 2,113 that do not, 1,453 fail
  to parse: 723 carry dashboard template syntax. In a sample of the other
  730, the failures are truncated rule text, invalid PromQL in the Promset
  fixture, and syntax specific to the Prometheus engine tests.
- Of the 6,208 lowered queries, 3,662 (59.0%) have at least one candidate with
  an ASAP operator (`SummaryAgg`). For 3,268 of them (52.6% of lowered) the
  only options are exact accumulators: no summary family applies.
- 2,107 lowered queries (33.9%) have no aggregate, and 434 (7.0%) aggregate
  only with intents that have no ASAP candidate.
- A summary candidate is valid for 318 queries, but for 286 of them only
  because a per-group count sketch reads its exact total. 32 queries (0.5% of
  lowered), all quantiles, have a valid approximate candidate; Stage 3
  selects a summary (always KLL) for 21 of them.
- Composition and Stage 2 implementation rejected nothing that Pass 1 offered,
  except one composition error (`summary input column does not resolve`, 2
  queries). Every loss after Pass 1 is a Stage 3 accuracy rejection.
- Across all composed candidates, the only ASAP operators that occur are
  `SummaryAgg`, `FinalizeExactAccumulator` and `SummaryEstimate`.
  `SummaryMerge`, `MaintainPopulation` and `EvaluatePopulation` never occur:
  single-query planning has no merge, and no corpus declares a maintained
  population.

### Intents per corpus

Queries containing each intent, among lowered queries. A query counts once per
intent. Columns group corpora: MO is metrics-observability.

| Intent | All | PromQL docs + testdata | o11y-bench | Alerts | MO logs | MO dashboards | MO rules + alerts | MO Promset | SQL packets + netflow | SQL TPC-H | SQL BGP |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Lowered queries | 6,208 | 1,121 | 27 | 947 | 342 | 780 | 408 | 2,345 | 77 | 50 | 111 |
| Sum | 1,913 | 187 | 22 | 238 | 136 | 405 | 186 | 713 | 13 | 1 | 12 |
| Rate | 1,212 | 103 | 13 | 246 | 104 | 64 | 125 | 557 | | | |
| Max | 455 | 15 | 8 | 23 | 38 | 105 | 76 | 172 | 10 | 5 | 3 |
| Increase | 439 | 21 | 5 | 80 | 25 | | 38 | 270 | | | |
| Avg | 412 | 86 | 1 | 20 | 14 | 15 | 13 | 226 | 9 | 28 | |
| Count | 288 | 35 | | 17 | 23 | 27 | 20 | 82 | 21 | 8 | 55 |
| HistogramQuantile | 233 | 50 | 3 | 29 | 24 | 15 | 37 | 75 | | | |
| Min | 133 | 15 | | 14 | 11 | 2 | 16 | 57 | 8 | 6 | 4 |
| IRate | 88 | 16 | | | 3 | 32 | 3 | 34 | | | |
| Changes | 65 | 16 | | 14 | 9 | 2 | 16 | 8 | | | |
| Absent | 60 | 16 | | 13 | 7 | | 12 | 12 | | | |
| Cardinality | 60 | | | | | | | | 29 | 5 | 26 |
| TopK | 51 | 3 | | | 5 | | 2 | 20 | 2 | | 19 |
| Deriv | 49 | 9 | | 8 | 3 | | 1 | 28 | | | |
| PredictLinear | 47 | 11 | | 4 | 9 | | 11 | 12 | | | |
| StdDev | 40 | 12 | | | 1 | | | 17 | 8 | 2 | |
| Quantile | 40 | 14 | | 1 | 1 | | | 17 | 2 | 5 | |
| Delta | 30 | 7 | | 11 | 1 | | | 11 | | | |
| AbsentOverTime | 27 | 21 | | | | | | 6 | | | |
| LastOverTime | 27 | 5 | | 3 | 3 | | 3 | 13 | | | |
| CountValues | 21 | 6 | | 1 | 1 | | 2 | 11 | | | |
| PresentOverTime | 18 | 17 | | | 1 | | | | | | |
| Variance | 17 | 11 | | | 1 | | 2 | 3 | | | |
| Resets | 17 | 16 | | | 1 | | | | | | |
| TimeFn | 14 | 12 | | | 1 | | | 1 | | | |
| IDelta | 10 | 10 | | | | | | | | | |
| Other (Group, MadOverTime, DoubleExpSmoothing, Ts*OverTime, FirstOverTime, Extension, PearsonCorr) | 29 | 20 | | | 1 | | 2 | | | 2 | 3 |

`FrequencyL2` and `FrequencyEntropy` occur in no corpus. `Cardinality` occurs
only in SQL (`COUNT(DISTINCT …)`, `uniqExact`, `APPROX_DISTINCT`): PromQL
distinct counts are written as `count(count by (x) (…))` and lower to two
`Count` aggregates.

### Shapes per corpus

| Shape | All | PromQL docs + testdata | o11y-bench | Alerts | MO logs | MO dashboards | MO rules + alerts | MO Promset | SQL packets + netflow | SQL TPC-H | SQL BGP |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| No aggregate | 2,106 | 478 | 1 | 448 | 75 | 286 | 72 | 719 | 2 | | 25 |
| One aggregate level | 2,718 | 579 | 6 | 310 | 148 | 335 | 163 | 990 | 72 | 50 | 65 |
| Nested aggregates (depth ≥ 2) | 1,384 | 64 | 20 | 189 | 119 | 159 | 173 | 636 | 3 | | 21 |
| Grouped (`by`/`without`/`GROUP BY`) | 1,934 | 144 | 8 | 248 | 162 | 354 | 229 | 654 | 67 | | 68 |
| Per-series window function | 2,558 | 463 | 24 | 396 | 191 | 99 | 236 | 1,149 | | | |
| Range selector | 2,405 | 364 | 23 | 382 | 183 | 99 | 224 | 1,130 | | | |
| Subquery | 170 | 97 | 3 | 1 | 1 | | 1 | 67 | | | |
| Offset | 158 | 97 | 2 | 9 | | | | 50 | | | |
| Binary operator over two operator inputs | 1,459 | 175 | 8 | 316 | 135 | 252 | 208 | 365 | | | |
| `Filter` (PromQL comparisons; SQL predicates not pushed into the scan) | 1,484 | 69 | 1 | 917 | 143 | 26 | 176 | 119 | 16 | | 17 |
| `Sort` or `Limit` | 267 | 41 | 1 | | 11 | 14 | 18 | 60 | 51 | | 71 |
| SQL window function | 17 | | | | | | | | 12 | | 5 |
| Join | 2 | | | | | | | | | | 2 |
| Scalar root | 80 | 47 | | | 1 | 13 | | 19 | | | |

The "No aggregate" shape row counts one query fewer than the outcome table:
one BGP query's `GROUP BY` without an aggregate function lowers to an
`Aggregate` with no measure, which the outcome table counts as no aggregate.

Window forms are PromQL range selectors and subqueries; SQL time windows are
`WHERE` time predicates. ClickHouse time buckets (`toStartOfHour`,
`toStartOfFiveMinutes`, `toDate`, …) are the first unsupported function in
32 BGP queries that fail lowering, so no corpus reaches the planner with a
SQL tumbling window.

### Families per intent

What Pass 1 offers (`summary_candidates` and the exact options), what Stage 3
accepted in this run, and what the executor in `asap-physical-operators`
implements (`capability.rs`: update kernels and native evaluation).

| Intent | Targets | Pass 1 exact option | Pass 1 summary families | Valid in Stage 3 (targets) | Executor support |
|---|---:|---|---|---|---|
| Sum | 2,717 | `ExactAccumulator` Sum | none | n/a | kernel and evaluation |
| Rate | 1,778 | `ExactAccumulator` Rate | none | n/a | kernel and evaluation |
| Max / Min | 535 / 148 | `ExactAccumulator` Max / Min | none | n/a | kernel and evaluation |
| Avg | 476 | `AvgAsSumCount` (`Exact` for 8 nullable inputs) | none | n/a | through Sum and Count |
| Increase | 474 | `ExactAccumulator` Increase | none | n/a | kernel and evaluation |
| Count | 320 | `ExactAccumulator` Count | CMS, Count Sketch, UnivMon | UnivMon 320; CMS and Count Sketch 129; selected 0 | kernels for all three; native bare-count evaluation only for CMS |
| IRate | 90 | `ExactAccumulator` IRate | none | n/a | **no exact kernel** |
| Cardinality | 70 | `Exact` | HLL, KMV, UnivMon (single column) | 0 | HLL kernel and evaluation; **no KMV kernel**; UnivMon has no cardinality evaluation |
| TopK | 52 | `Exact` | CMS+heap, Count Sketch+heap (42 targets; 10 pruned as exact-only) | 0 | heap kernels and keyed evaluation |
| Quantile | 47 | `Exact` | KLL, DDSketch | 32 each; KLL selected for 21 queries | kernels and evaluation |
| HistogramQuantile, Changes, Absent, Deriv, PredictLinear, StdDev, Variance, Delta, LastOverTime, CountValues, Resets, IDelta, … | 0 | none (not a target) | none | n/a | n/a |

The whole-expression top-k candidate is offered for no corpus query.

## Gaps ranked by impact

Counts are measured unless marked "upper bound". "Blocks" means the gap is
the reason the query has no valid ASAP summary candidate today; "affects"
means it limits a query that already has exact accumulators.

| Rank | Gap | Queries | Primitive or rule needed |
|---:|---|---:|---|
| 1 | Additive and extremal intents have only exact accumulators, and Pass 2 shares only identical expressions | affects 3,268 | Pass 2 window-composition and summary-capability rules over exact accumulators and summaries (`SummaryMerge` of panes), and Stage 1 search that does not enumerate the full product |
| 2 | Window-function and presence intents have no ASAP candidate | blocks 434 | Mergeable exact accumulators: moments, first/last sample, regression sums, change and reset counts, presence |
| 3 | Native histograms are not represented | blocks 371 (370 in engine testdata) | A native histogram sample type and a mergeable bucket summary |
| 4 | Count summaries are per-group totals | 286 valid but never useful | A keyed frequency summary over the group key (shared grouping) |
| 5 | Value predicates and ordering above approximate outputs void guarantees | blocks 37 of 76 rejected; 1,484 queries contain a `Filter` | Guarantee composition through `Sort`, and a decision guarantee for threshold comparisons |
| 6 | Frontend coverage (excluding native histograms) | blocks 289 | ClickHouse builtins and time buckets; PromQL `info()`, `fill()` matching, `__name__` regex |
| 7 | IRate exact accumulator has no executor kernel | affects 88 | An IRate kernel (last two samples per series) |
| 8 | Distinct counts never certify | blocks 52 of 60 (no target of 70 is valid) | Distinct-count evidence for HLL; KMV sizing for δ and a KMV kernel; a UnivMon cardinality bound |
| 9 | Top-k never certifies | blocks 16 of 51 | Counter declarations for CMS+heap weights; a membership bound for heap sketches |

### 1. Sharing for exact-accumulator queries

Half of the lowered queries are sums, rates, increases, maxima, averages and
minima, usually per-series window functions (`rate(m[5m])`) under a grouped
aggregate. Pass 1 offers each only its exact accumulator: there is no summary family
for these intents. Planning one query at a time leaves nothing to choose.
The savings for these queries come from Pass 2, across queries:

- The identical-expression rule (implemented) applies to 1,441 queries whose
  aggregate also occurs in another query of the same corpus. It cannot be
  used at corpus scale: the Pass 1 product of 11 of 18 corpora exceeds the
  10⁴ Stage 1 limit (up to 10⁷⁰ for Promset), before Pass 2 subsets
  multiply it further.
- The window-composition rule (not implemented) applies to at most 433
  queries whose input matches another query's except for window lengths,
  with the same intent and grouping (`rate(x[1m])` and `rate(x[5m])`).
- The summary-capability rule (not implemented) applies to at most 1,290
  queries whose aggregate input is also read by a different aggregate in
  another query (`sum by (a)` and `count by (a, b)` over one selector). The
  rule needs one summary that answers both; this count does not check that.

#### Pass 2 sharing potential

| Corpus | Queries with a target | Identical-expression | Summary-capability (upper bound) | Window-composition (upper bound) | log₁₀ of Pass 1 product |
|---|---:|---:|---:|---:|---:|
| PromQL docs examples | 20 | 8 | 2 | 0 | 1.1 |
| PromQL engine testdata | 422 | 144 | 193 | 69 | 28.2 |
| o11y-bench PromQL | 26 | 6 | 2 | 5 | 0.0 |
| awesome-prometheus-alerts | 446 | 64 | 6 | 11 | 14.3 |
| MO logs A | 162 | 39 | 2 | 14 | 12.0 |
| MO logs B | 72 | 2 | 3 | 2 | 6.7 |
| MO dashboards A | 174 | 90 | 3 | 0 | 4.8 |
| MO Grafana dashboards | 164 | 72 | 66 | 0 | 12.0 |
| MO Kubernetes-mixin dashboards | 155 | 111 | 10 | 0 | 1.2 |
| MO rules A | 202 | 129 | 0 | 16 | 12.6 |
| MO Kubernetes-mixin alerts | 29 | 13 | 4 | 0 | 3.8 |
| MO Kubernetes-mixin rules | 66 | 29 | 2 | 14 | 0.0 |
| MO Promset | 1,530 | 724 | 889 | 302 | 70.2 |
| SQL packet-trace fidelity | 64 | 8 | 56 | 0 | 27.8 |
| SQL TPC-H data-quality checks | 48 | 0 | 22 | 0 | 27.3 |
| SQL netflow | 7 | 0 | 6 | 0 | 2.6 |
| SQL BGP analytics | 5 | 0 | 0 | 0 | 3.0 |
| SQL BGP analyst workload | 78 | 2 | 24 | 0 | 57.8 |
| **Total** | **3,670** | **1,441** | **1,290** | **433** | |

The dashboard and rule corpora are real workloads (panels evaluated together,
rules evaluated on one interval); in the other corpora a shared fingerprint
only shows that the shape recurs.

### 2. Intents without an ASAP candidate

These intents are not Pass 1 targets, so the query is planned exactly. A row
counts the queries that contain one of its intents and have no Pass 1 target
at all (blocked), out of all queries that contain one of its intents. A query
can appear in several rows.

| Intent | Blocked / containing | Mergeable state that would answer it |
|---|---:|---|
| Changes, Resets | 71 / 82 | change or reset count plus the first and last sample of each pane |
| Absent, AbsentOverTime, PresentOverTime | 93 / 105 | a presence bit per series |
| Deriv, PredictLinear | 80 / 95 | regression sums (n, Σt, Σv, Σtv, Σt²) |
| StdDev, Variance | 50 / 57 | moments (n, Σv, Σv²) |
| Delta, IDelta, LastOverTime, FirstOverTime | 63 / 69 | first and last (and second-to-last) sample |
| CountValues | 16 / 21 | a value-frequency map, or a frequency sketch keyed by value |
| HistogramQuantile (classic buckets) | 25 / 233 | none needed: in the other 208 the inner `sum by (le) (rate(…))` is already a target |
| Other (TimeFn, Group, MadOverTime, DoubleExpSmoothing, Ts*OverTime, Extension, PearsonCorr) | 36 / 40 | varies; `MadOverTime` needs a quantile summary |

`Quantile`, `TopK` and `Cardinality` have no exact accumulator either: when
their summaries are rejected they run as plain exact aggregates.

### 3. Native histograms

371 queries fail lowering on native histograms: `histogram_fraction` (181),
`histogram_quantile` over series without classic buckets (109), and other
native histogram functions (81). All but one are in the engine testdata, so
the gap matters for conformance more than for the real-world corpora.

### 4. Count summaries are per-group totals

Pass 1 offers CMS, Count Sketch and UnivMon for every `count`. Composition
builds one summary per group (`PerSubpopulationInstance`) and reads its total
update count, which the exact count accumulator answers more cheaply. UnivMon
reads that total exactly (provenance `univmon_unit_update_total`) and is valid
for all 320 count targets; CMS and Count Sketch are valid for 129. Stage 3
never selects any of them. A count summary saves state only when one sketch
is keyed by the group key across groups. That is a shared grouping, and the
executor's kernel check (`validate_summary_kernel`) rejects every shared
grouping today.

### 5. Guarantees through comparisons and ordering

Stage 3 rejects an approximate candidate whose root guarantee is `None`. A
value comparison, a `HAVING`-style `Filter` or a `Sort` over an approximate
value yields `None`. Of the 76 queries whose summaries are all rejected, 35
are grouped distinct counts with `ORDER BY` over the estimate (the same query
without `ORDER BY` keeps the HLL bound), and two are quantiles under
`topk(1, …)`, which lowers to `Sort` and `Limit`. More broadly, 917 of
the 947 lowered alerts contain a `Filter`, almost always the alert threshold:
a summary family added for their sums and rates would be rejected the same
way.

### 6. Frontend coverage

| Cause | Queries |
|---|---:|
| SQL: ClickHouse scalar functions without a registered signature (`toDate`, `toStartOf*`, `splitByChar`, …) | 54 |
| SQL: DataFusion planning (unknown ClickHouse functions, list access, grammar) | 50 |
| PromQL: series identity for `info()` | 55 |
| PromQL: `fill()` vector-matching modifiers | 44 |
| PromQL: `__name__` regex or non-equality matchers | 44 |
| PromQL: other (non-constant parameters, `step()`/`range()`, string literals, unresolved columns, …) | 42 |
| PromQL: native histograms (rank 3) | 371 |

### 7. IRate

Pass 1 offers an `IRate` exact accumulator for 90 targets in 88 queries, and
Stage 3 accepts it, but the executor has no `IRate` kernel. Every other exact
accumulator Pass 1 offers has one.

### 8–9. Distinct count and top-k

- Global `COUNT(DISTINCT x)`: HLL's bound is 0.0081, but its failure
  probability needs a distinct-count bound for the source, and the run
  declares none. KMV, sized for the target, reports failure probability 0.01
  against δ = 0.001; the executor has no KMV kernel. UnivMon has no guarantee
  for a distinct count.
- Top-k over `sum(rate(…))`: CMS+heap needs update weights proven
  non-negative, which requires the metric to be declared a counter; Count
  Sketch+heap has no guarantee because a heap does not bound membership. For
  10 targets the top-k itself carries an exact requirement (for example
  `topk` over a `sum` of a gauge, or SQL `ORDER BY SUM(…) LIMIT`), so Pass 1
  prunes both heap families.

## Recommendations

In order of corpus queries reached per unit of work:

1. **Window-composition and summary-capability rules for exact accumulators**,
   with pane `SummaryMerge`, and a Stage 1 search that prunes before it
   enumerates. These reach the 3,268 exact-accumulator queries, which are the
   dashboards and rules that run together. Measure first on the corpora whose
   Pass 1 product is below 10⁴ (o11y-bench, Kubernetes-mixin dashboards,
   alerts and rules, docs examples, netflow, BGP analytics).
2. **Mergeable exact accumulators for window functions**: moments,
   first/last sample, regression sums, change/reset counts and presence. Each
   is small and composes with item 1; together they make 434 more queries
   eligible.
3. **An IRate kernel** (88 queries), so every exact accumulator Pass 1 offers
   can run.
4. **Keyed count summaries** in place of per-group count sketches, or stop
   offering per-group count sketches that cannot beat the exact count.
5. **Guarantee propagation through `Sort`** (value-preserving) and a decision
   guarantee for threshold comparisons, before adding approximate families
   for alerting intents.
6. **Distinct-count and top-k evidence paths**: declare distinct-count bounds
   and counter metrics in the data workloads that the evaluation uses; fix
   KMV sizing against δ and add its kernel.

Native histograms and the frontend gaps are real but reach mostly the engine
testdata (PromQL) and one ClickHouse corpus (SQL).

## Limitations

- One query per workload. Cross-query sharing is estimated from fingerprints,
  not planned; the summary-capability and window-composition counts are upper
  bounds that do not check whether one summary answers both computations.
- One accuracy target for every query (ε = 0.01, δ = 0.001) and no
  data-workload evidence. With distinct-count bounds or counter declarations
  some HLL and CMS+heap candidates would become valid; that case was not run.
- Validity is Stage 3's accuracy check. Stage 2 does not check executor
  kernels, so "valid" includes families the executor cannot run (KMV, IRate,
  UnivMon readouts). The executor column comes from `capability.rs`, not from
  running plans.
- Corpus quality varies: engine testdata exercises edge cases, Promset
  contains invalid PromQL, and dashboard fixtures keep template variables.
  Template expansion was not attempted.
- Intent classification is by `AggIntent` variant. A PromQL distinct count
  written as nested `count` is counted as `Count`.
- The sweep driver is not in the repository; the per-query pipeline is
  (`stage_pipeline`).

## Related work

- [planner-layering.md](planner-layering.md): stages, Pass 1 and the Pass 2
  rules.
- [univmon-frequency-summary.md](univmon-frequency-summary.md): UnivMon
  readouts and their accuracy gating.
- [asapquery-rule-coverage.md](asapquery-rule-coverage.md): rule coverage
  against ASAPQuery.
- [metrics-observability-corpora.md](../../develop_docs/metrics-observability-corpora.md):
  how the metrics-observability fixtures are extracted.
