# Shared maintained population rule

## Definition and motivation

A **population** is the multiset of records that an aggregation is defined over,
after applying its source selection, predicates, membership semantics and grouping.
A **maintained population** is that multiset represented by state which is kept
across evaluations and updated when members enter, change, leave or expire.
A **readout** computes a result from the maintained state at an admitted evaluation.

For group `g` and evaluation `t`, write this multiset as `P_g(t)`. The contract is
that `ReadPopulation(f, t)` returns `f(P_g(t))`; it must not read a partial or stale
population outside the deployment's admitted coverage/freshness contract. The
population describes **which records count**. The physical data structure describes
**how those records are retained and read**.

For example, suppose two live PromQL series currently have values `7` and `7`.
Their population contains two members: `count(a)` is `2`, not `1`. If the first
series changes to `9`, the population becomes `{9, 7}`, not `{7, 7, 9}`. Its previous
value is replaced. This distinction requires an explicit rule and state contract:
an append-only quantile sketch cannot by itself implement current-series updates.

The current rule retains an exact population. It does not prescribe a particular
tree, heap or sketch implementation, and it does not imply a deletable DDSketch.

## Membership semantics

| Input contract | Members | Membership changes |
| --- | --- | --- |
| `CurrentSeries` | Latest live sample for each matching series at `t`, partitioned by the declared labels | A newer sample replaces that series' member; a stale marker removes it; lookback expiry removes it |
| `Rows` | Every row of the declared table input, preserving duplicate multiplicity and applying its predicates/grouping | Inserts add members, updates replace affected members, deletes remove the corresponding occurrences; a complete snapshot can atomically replace the multiset |

The canonical PromQL contract uses a five-minute lookback. For Prometheus 3.5,
the valid sample interval is `(t - 5m, t]`: a sample exactly at the lower boundary
is expired. This is a membership requirement, not a configurable sketch window.
SQL table rows do not inherit this lookback, series identity, or stale-marker behavior.
For example, two historical rows belonging to one device still count as two SQL
rows unless the SQL plan explicitly selects the latest row per device.

`CurrentSeriesInput` carries the metric, label matchers, grouping and lookback.
`Rows` carries the canonical input (including predicates/schema), value-column
index and grouping. Source identity, predicates, membership semantics, value
column and grouping determine whether consumers refer to the same population.

## Rule: share one population across compatible readouts

**Implementation:** `MaintainedPopulationStrategy`, an opt-in `ReplacementStrategy`
in [maintained_population.rs](../../../crates/asap-aware-mapping/src/maintained_population.rs).

**Target sub-DAGs:**

- A single-measure `Aggregate(Reduce(grouping), input)` with Quantile, Sum, Count
  or Average intent and no HAVING clause.
- A descending single-column `Sort(input)` followed by `Limit(k, offset=0)`.
- SQL projections above these targets are preserved in the replacement.

The supported input is a canonical direct scan with one of the membership
contracts above. Current-series scans must have the canonical open time-series
schema and supported label predicates. Table scans require a closed schema and
a non-null Float64 value column. Arbitrary relational inputs, nullable value
columns and multi-measure aggregates need additional rules.

**Replacement sub-DAG:**

```text
KeepPreAsap(input)
  -> MaintainPopulation { input, max_k, quantiles }  [maintenance]
       -> ReadPopulation { Quantile(q1) }           [read]
       -> ReadPopulation { Quantile(q2) }           [read]
       -> ReadPopulation { TopK(k1) }               [read]
       -> ReadPopulation { TopK(k2) }               [read]
       -> ReadPopulation { Sum | Count | Average }  [read]
```

The rule examines compatible workload roots, sets `max_k` to the largest requested
k and enables quantile readout if any consumer needs it. It emits a candidate for
each root; canonical summary CSE interns their identical maintenance producers.
The readout rank `q` and requested prefix `k` do not identify different input
populations. The union of readout requirements does affect the shared producer's
configuration, retained memory and cost.

**Concrete transformation:**

```promql
quantile(0.5, a)
quantile(0.99, a)
topk(1, a)
topk(5, a)
```

These queries can use one `CurrentSeries` producer with `max_k=5` and quantile
readout enabled. The full population remains available: deleting a TopK member
must allow a previously lower-ranked member to be promoted. Retaining only the
largest five values would not preserve that behavior.

The same rule can represent these SQL consumers using a `Rows` producer:

```sql
SELECT median(latency) FROM samples;
SELECT approx_percentile_cont(latency, 0.99) FROM samples;
SELECT * FROM samples ORDER BY latency DESC LIMIT 1;
SELECT * FROM samples ORDER BY latency DESC LIMIT 5;
```

By contrast, `a{job="api"}` and `a{job="db"}`, different value columns, and
`by(job)` versus `by(region)` identify different populations and are not shared
by this rule. SQL rows and PromQL current-series members never share state merely
because their source names or numeric values happen to agree.

## Validation, selection and execution responsibilities

Planner validates the declared input, maintenance/read phases and readout
compatibility. Its intended guarantee is exact membership and exact readout;
a physical implementation still must preserve the language's numeric and empty-input
semantics. In particular, SQL global COUNT over an empty population returns a row
with zero, while PromQL COUNT over an empty vector returns an empty vector.

The rule proposes a candidate; it does not select it unconditionally. A compiler
must lower the typed DAG only if its executor supports that membership contract.
Installation requires complete cost evidence for population construction, updates,
retention, readouts, retirement and any required raw-data work. Shared state is
not automatically cheaper than independent or native execution.

The executor owns record identity, input completeness, replacement/retraction,
coverage, freshness, atomic publication and resource limits. Missing coverage,
unsupported semantics or exhausted resources must not produce a partial result
advertised as exact. The backend's current-series implementation rejects evaluations
older than retained state and falls back while coverage is insufficient.

The backend has separate executors for current-series membership and SQL table
snapshots. Its SQL `Rows` executor polls a complete ClickHouse scan and atomically
replaces the row multiset. Multiple quantile/TopK readouts share one background
maintenance task and committed epoch, including after table updates and deletes.
The deployment must explicitly configure the source database, refresh interval,
maximum snapshot age and resource budget; each SQL request must accept that age
bound. Missing, failed or stale snapshots use native execution. This is bounded-stale
snapshot maintenance, not a database change log or implicit transaction freshness.
Existing SQL window-summary compilation remains separate. Complete cost selection
must price every scan/transfer, reconciliation, residency, readout and fallback.

## Relation to sketch rules and other optimizations

This rule adds an exact maintained-state alternative. It is distinct from choosing
a sketch family or merging temporal panes, and can coexist with those alternatives
in the same workload. Temporal sketch rules continue to emit
`SummaryAgg -> SummaryEstimate` DAGs.

For example, `distinct_over_time(a[5m])`, `l2_over_time(a[5m])` and
`entropy_over_time(a[5m])` can read one UnivMon frequency summary when input,
partitioning, window and sketch parameters match. Here L2 is
`sqrt(sum_v count(v)^2)`, and entropy is computed from the same value frequencies.
Each readout still needs its own accuracy evidence: sharing an entropy certificate
does not establish a cardinality or L2 bound. This is the same separation of
population/state identity from readout identity, implemented by the existing sketch
rules rather than by converting UnivMon into an exact `MaintainPopulation` node.

## Acceptance evidence

- PromQL quantiles, TopK limits and scalar readouts share only compatible populations.
- SQL frontend tests cover shared quantiles/scalar readouts/maximum k, separation
  by grouping and filters, preservation of projections, and invalid value-column rejection.
- Backend admission requires the matching SQL executor and explicit snapshot policy;
  a table-row producer can never use the remote-write current-series executor.
- ClickHouse integration compares quantile endpoints/interpolation, Sum, Avg, Count
  and multiple TopK limits across inserts, updates, single-member deletion and an
  empty table, including native result metadata and freshness/failure fallback.
- Process tests compare current-series replacements and expiry with Prometheus 3.5.
- The UnivMon process test installs one compatible materialization for all three
  readouts and checks that missing entropy evidence does not disable the L2 path.

These tests establish the covered semantic and sharing behavior, not measured
end-to-end speedups or universal floating-point equivalence. The review regressions
for the exact lookback boundary and temporal-average overflow are separate checks;
passing the ordinary workload examples alone does not establish those edge cases.
