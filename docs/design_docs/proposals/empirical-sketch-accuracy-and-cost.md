# Empirical sketch accuracy and cost from sketch-bench

> Status: proposal, for review. Nothing here is implemented yet except the
> existing cost converter described in "What exists today". The decisions in
> "Decisions" are settled; the two open questions are not.

This design uses sketch-bench as the **empirical source of sketch accuracy**
and as a **cross-check for cost** in ASAPPlanner. It adds a generic
measured-accuracy table to plan selection. The Planner reads only that table
and the existing measured cost table; it never depends on sketch-bench, its
crates or its file formats. Conversion lives in the Planner's devtools.

Motivating issues: [#798](https://github.com/ProjectASAP/ASAPPlanner/issues/798)
(sizing allocates far larger sketches than the workload needs),
[#322](https://github.com/ProjectASAP/ASAPPlanner/issues/322) (feed empirical
sketch error and resource usage into planner estimates) and
[#791](https://github.com/ProjectASAP/ASAPPlanner/issues/791) (a partial cost
table mis-ranks plans).

## Contents

1. [Problem, by example](#1-problem-by-example)
2. [Goals and non-goals](#2-goals-and-non-goals)
3. [What exists today](#3-what-exists-today)
4. [Components](#4-components)
5. [Data formats](#5-data-formats)
6. [Algorithm](#6-algorithm)
7. [Code interface](#7-code-interface)
8. [Decisions](#8-decisions)
9. [Open questions](#9-open-questions)
10. [Implementation plan](#10-implementation-plan)
11. [Test plan](#11-test-plan)
12. [Risks and limitations](#12-risks-and-limitations)
13. [Relation to other designs](#13-relation-to-other-designs)

## 1. Problem, by example

A dashboard asks for the per-series p99 latency over the last five minutes,
`quantile_over_time(0.99, latency[5m])`, with this accuracy target:

```rust
AccuracyTarget::max_error(0.0018)
    .failure_probability(0.01)
    .accuracy_metric(AccuracyMetric::Rank)
    .accuracy_scope(AccuracyScope::PerValue)
```

Pass 1 replaces the exact quantile with a KLL summary and sizes it from the
analytical inverse `kll_k(ε)`, which inverts the registered 99% fit
`ε = 2.296 / k^0.9723`:

| k | Analytical rank error at δ = 0.01 | Measured on the executor's KLL |
| --- | --- | --- |
| 800 | 0.00345 | 0.00176 |
| 1,564 | 0.00180 | not measured |

`kll_k(0.0018)` is **k = 1,564**. An empirical check of the executor's own
KLL found that the analytical bound holds (the measured failure probability
at the bound is at most 0.0054 for δ = 0.01, and the bound is conservative at
δ = 0.1 and 0.3), but that it is about **2× loose at large k**: k = 800
already reaches a rank error of 0.00176, below the 0.0018 target. The
Planner therefore allocates roughly twice the KLL state the target needs, for
every series, and Stage 3 prices and compares that larger state.

Nothing in the Planner can make it smaller today:

- Stage 1 sizes only from the analytical inverses.
- The only evidence hook, `AccuracyEvidenceProvider::output_guarantee`, judges
  a whole query output, and Stage 3 reads it only when the analytical
  guarantee *fails*. The analytical guarantee of k = 1,564 already meets the
  target, so the evidence is never read.
- Stage 3's failure-probability resize only grows summaries.

So evidence can accept a candidate that has no analytical guarantee, but it
can never shrink one that has. This design lets measured accuracy pick
**k = 800**, a configuration that was actually measured, and keeps the
analytical size as the fallback whenever no measurement applies.

The same gap exists for cost. The Planner can read sketch-bench's measured
per-operation CPU, but the conversion drops memory, exact accumulators and
provenance, and sketch-bench measures no relational operators, so a table
built from it alone can mis-rank exact plans against sketch plans (#791).

## 2. Goals and non-goals

**Goals**

1. Plan selection can use a measured tail error, `Pr[err > ε] ≤ δ`, for a
   summary configuration that was actually measured, and choose it when it is
   cheaper than the analytically sized one.
2. The analytical accuracy models stay as the worst-case fallback; the
   Planner never extrapolates a measurement.
3. Every measured number carries provenance: the library rev it was measured
   on, the benchmark commit and the data shape.
4. The cost path from sketch-bench carries memory per state, exact
   accumulators and provenance, and refuses a library rev mismatch.
5. Validate the result end to end: re-plan the ASAPQuery workloads and compare
   predicted with measured cost and error.

**Non-goals**

- The Planner does not depend on sketch-bench or on its `rqe-optimizer` crate.
- No shape-conditioned lookup: measurements are at one canonical shape until
  `DataWorkload` declares data shape.
- No measured accuracy for top-k, Count-Min, Count Sketch, UnivMon, KMV or
  Hydra structures in this design (see "Decisions").
- No runtime feedback of observed error into re-planning.

## 3. What exists today

Checked against ASAPPlanner `453a6da2` and sketch-bench `a3753f2`.

| Piece | Where | What it does, and its gap |
| --- | --- | --- |
| Cost converter | `crates/devtools/src/rqe_atomic_costs.rs` (`convert_rqe_atomic_costs`), bin `convert_rqe_atomic_costs` | Turns each `rqe_atomic_costs.json` row into three entries (update, merge, estimate) in the earlier array format. It drops `mem_bytes_per_instance`, skips `exact-*` and `hydra-*` rows, and writes no provenance and no phase. |
| Measured cost table | `crates/plan-selection/src/measured_operation_costs.rs` (`MeasuredOperationCosts`, `OperatorCost`, `CostTableProvenance`) | Already supports `memory_bytes_per_state`, `exact`, `phase` and `provenance { executor_version, machine }`. The converter uses none of them. The array format loads as `OperatorCost` entries at both phases. |
| Consumers of the cost table | `stage_pipeline --measured-operation-costs`, `PlanSelectionSettings::measured_operation_costs`, the backend `control_plane --measured-operation-costs` | A node with an entry is priced from it; every other node from the formulas times **one** calibration factor (#791). |
| Accuracy evidence hook | `crates/plan-selection/src/accuracy.rs`, `AccuracyEvidenceProvider::output_guarantee` | A whole-output guarantee, read in `guarantee_check` only when the analytical root guarantee does not meet the target. |
| Failure-probability resize | `resize_for_failure_probability` in `crates/plan-selection/src/lib.rs` | Splits a query's δ over its `n` approximate inputs and `G` values (`δ / (n · G)`), keeps the smallest share per shared state, and only enlarges parameters (`with_failure_probability` returns the input unchanged when the new δ is not smaller). |
| Analytical sizing | `realization::default_size_params` → `accuracy::estimators::size_params` (e.g. `kll::kll_k`) | Stage 1 sizes from ε and δ; it reads no data workload. |
| Shared sizing | `size_for_all_readers` (Pass 2's summary-capability rule) | A shared state takes each parameter's largest value among its readers (a KLL's δ, DDSketch's α: the smallest). |
| Guarantee provenance | `GuaranteeSource::SketchEvaluation { algorithm, contract, params, query }` in `crates/types/src/ir/properties/guarantee.rs` | `contract` is a free string, e.g. `"apache_datasketches_kll_empirical_99_a9b42755072b"`. It is written by the estimators (`estimators/mod.rs`, `kll.rs`, `hll.rs`, `univmon.rs`) and read only by tests. |
| Library pin | workspace `Cargo.toml`: `asap_sketchlib` rev `63e5655` (0.4.0 and later) | The executor's kernels. sketch-bench benchmarks crates.io `asap_sketchlib` 0.3.0. Neither records the rev with its numbers. |
| Frequency variants | executor `summary_kernels/weighted_frequency.rs` | The executor runs `RegularPath` Count-Min and Count Sketch over `f64` counters. sketch-bench's cost rows use `FastPath` integer counters, a different kernel. |
| sketch-bench accuracy | `scripts/study_saturation.py --phase accuracy` and merge curves | Curves of `seed_mean_error`, a mean over seeds. Merge curves exist up to m = 16 merged states. Its theory fallback is a 95% two-sided bound. |
| sketch-bench cost | `scripts/study_saturation.py --phase optimizer-cost` → `rqe_atomic_costs.json` | Families `topk`, `cardinality` and `quantile` only; one canonical shape: Zipf θ = 1.1 over K = 10⁴ keys, Pareto a = 2 values, N = 10⁶ items. No relational operators (Scan, Filter, TimeRange, Aggregate). |
| Workload facts | `DataWorkload`, `SourceFacts` in `crates/types/src/workload/mod.rs` | Series count, sample interval, distinct bound, metric type, arrival rate. No data-shape facts. |

The executor runs Cms, CountSketch, CountSketchWithHeap, CmsWithHeap, Kmv,
UnivMon, HydraCms, KLL, DDSketch and HLL. HydraKLL is out of scope here.

sketch-bench already depends on ASAPPlanner crates, so the reverse dependency
would be a cycle (see "Decisions", D1).

## 4. Components

```text
            sketch-bench (producer, benchmarks asap_sketchlib at the executor's rev)
              |  rqe_atomic_costs.json            |  tail-accuracy export
              v                                   v
  asap-devtools: convert_rqe_atomic_costs    asap-devtools: convert_sketch_accuracy
   (rev check, memory, exact, provenance)     (rev check, statistic and δ checks)
              |                                   |
              v                                   v
     MeasuredOperationCosts (JSON)        MeasuredSketchAccuracy (JSON)
              \                                   /
               \                                 /
                 PlanSelectionSettings (Stage 3)
                   |                         |
                   v                         v
     pricing (per-class calibration)   MeasuredAccuracyEvidence -> Stage 3 sizing
                                            (measured smaller configuration,
                                             analytical fallback)
```

| Component | Owner | Role |
| --- | --- | --- |
| **sketch-bench** | sketch-bench | Runs `asap_sketchlib` at the executor's pinned rev, records that rev, and exports two files: per-operation cost (`rqe_atomic_costs.json`) and tail accuracy. It knows nothing about Planner types. |
| **Converters** | ASAPPlanner `asap-devtools` | Read sketch-bench's files and write the Planner's tables. They are the only Planner code that reads a sketch-bench format, and they refuse a library rev other than the executor's. |
| **`MeasuredOperationCosts`** | ASAPPlanner `plan-selection` (exists) | The deployment's measured cost table. Gains library rev and producer in its provenance. |
| **`MeasuredSketchAccuracy`** | ASAPPlanner `plan-selection` (new) | A generic table of measured tail errors per sketch configuration, statistic, scope, state size and merge count. Any producer can write it; sketch-bench is one. |
| **`MeasuredAccuracyEvidence`** | ASAPPlanner `plan-selection` (new) | An `AccuracyEvidenceProvider` that answers summary-level lookups from the table and delegates everything else to the deployment's own provider. |
| **Stage 3 sizing** | ASAPPlanner `plan-selection` | Tries the measured configurations that meet each reader's target, cheapest first, before the analytical size. |
| **Executor** | ASAPPlanner `executor` | Its `asap_sketchlib` pin is authoritative. It exposes the rev as a constant the converters check. |
| **Validation harness** | local, outside the repo | Re-plans the ASAPQuery workloads three ways and compares with the measured runs. |

Ownership follows the existing boundary of the measured cost table: the
Planner defines the structure, the deployment or a benchmark measures the
numbers, and only devtools reads a foreign format. The Planner stays
deployment-agnostic: neither table names a dataset, tenant or host.

## 5. Data formats

### 5.1 Measured-accuracy table

`PlanSelectionSettings::measured_sketch_accuracy`, loadable on its own with
`stage_pipeline --measured-sketch-accuracy <file>`.

```json
{
  "provenance": {
    "library": "asap_sketchlib",
    "library_rev": "63e565506426020c505ef0b84e0c91f450d09e7c",
    "producer": "sketch-bench",
    "producer_commit": "<sketch-bench commit>",
    "data_shape": {
      "key_distribution": {"zipf": {"theta": 1.1, "keys": 10000}},
      "value_distribution": {"pareto": {"alpha": 2.0, "scale": 1000.0}},
      "items": 1000000
    }
  },
  "entries": [
    {
      "sketch": {"Kll": {"k": 800}},
      "statistic": {"Quantile": {"q": 0.99}},
      "accuracy_metric": "Rank",
      "accuracy_scope": "PerValue",
      "groups": 1,
      "samples_per_state": 1000000,
      "distinct_per_state": null,
      "merged_states": 1,
      "trials": 1000,
      "error_at": [
        {"failure_probability": 0.01, "error": 0.00176},
        {"failure_probability": 0.05, "error": "<measured>"},
        {"failure_probability": 0.3,  "error": "<measured>"}
      ]
    },
    {
      "sketch": {"Kll": {"k": 800}},
      "statistic": {"Quantile": {"q": 0.99}},
      "accuracy_metric": "Rank",
      "accuracy_scope": "WholeResult",
      "groups": 100,
      "samples_per_state": 10000,
      "distinct_per_state": null,
      "merged_states": 16,
      "trials": 1000,
      "error_at": ["..."]
    }
  ]
}
```

Fields of an entry:

| Field | Meaning |
| --- | --- |
| `sketch` | The exact `SketchParams` measured. Only these configurations can be chosen. |
| `statistic` | The `SketchStatistic` read out (`Quantile { q }`, `Cardinality`). A quantile entry applies only to the same `q`. |
| `accuracy_metric` | The metric of `error`: `Rank` for KLL, `Relative` for DDSketch and HLL. |
| `accuracy_scope` | `PerValue`: the error of one group's value. `WholeResult`: per trial, the maximum error over the `groups` groups measured. |
| `groups` | The number of groups `G` of a whole-result entry; 1 for a per-value entry. |
| `samples_per_state` | Rows inserted into each state before it is merged or read. |
| `distinct_per_state` | For `Cardinality`: the distinct keys per state; the axis an HLL lookup uses. `null` otherwise. |
| `merged_states` | States merged before the readout, `m` (1 = no merge). |
| `trials` | Independent trials (seeds) behind each quantile. |
| `error_at` | For each `failure_probability` δ in {0.01, 0.05, 0.3}: the empirical `(1 − δ)` quantile of the trial errors, i.e. the error exceeded in a fraction δ of trials. Never a mean. |

Validation (bad input, the whole table is refused):

- a δ other than 0.01, 0.05 or 0.3, or a δ with `δ < 3 / trials` (0.01 needs
  at least 300 trials);
- an error that is negative, non-finite, or smaller at a smaller δ (the tail
  must not shrink as δ shrinks);
- a family other than KLL, DDSketch and HLL, or a metric that does not match
  the family;
- two entries with the same key (sketch, statistic, scope, groups,
  samples_per_state, distinct_per_state, merged_states);
- no provenance, or an empty `library_rev`.

The Planner does not compare `library_rev` with its own pin: that check is the
converter's (D2). The rev is carried into every guarantee built from the
table.

### 5.2 Extended operation-cost table

The converter writes the object form `MeasuredOperationCosts` already loads,
instead of the earlier array:

```json
{
  "operators": [
    {"operator": "SummaryAgg", "phase": null, "sketch": {"Kll": {"k": 200}},
     "ns_per_unit": 85.0, "memory_bytes_per_state": 9648.0},
    {"operator": "PaneChain", "phase": null, "sketch": {"Kll": {"k": 200}},
     "ns_per_unit": 85.0, "memory_bytes_per_state": 9648.0},
    {"operator": "SummaryMerge", "phase": null, "sketch": {"Kll": {"k": 200}},
     "ns_per_unit": 1200.0},
    {"operator": "SummaryEstimate", "phase": null, "sketch": {"Kll": {"k": 200}},
     "ns_per_unit": 400.0},
    {"operator": "SummaryAgg", "phase": null, "exact": "Sum",
     "ns_per_unit": 12.0, "memory_bytes_per_state": 56.0},
    {"operator": "FinalizeExactAccumulator", "phase": null, "exact": "Sum",
     "ns_per_unit": 3.0}
  ],
  "ns_per_evaluation": 0.0,
  "provenance": {
    "executor_version": "asap_sketchlib 63e565506426020c505ef0b84e0c91f450d09e7c",
    "machine": "<CPU model>",
    "library_rev": "63e565506426020c505ef0b84e0c91f450d09e7c",
    "producer": "sketch-bench <commit>",
    "data_shape": "zipf(theta=1.1, keys=1e4), pareto(alpha=2), N=1e6"
  }
}
```

Changes against today's output:

- `memory_bytes_per_state` from the row's `mem_bytes_per_instance`, on the
  state-holding operators (`SummaryAgg`, `PaneChain`).
- `exact-*` rows become entries with `exact` set (`exact-sum` → `Sum`,
  `exact-min` → `Min`, `exact-max` → `Max`, `exact-increase` → `Increase`);
  their estimate becomes `FinalizeExactAccumulator`. A row with no `ExactKind`
  (e.g. `exact-delta-set`) stays skipped with its reason.
- `hydra-*` rows stay skipped: they are one structure shared across groups,
  measured at one label width, and the table prices one state per group.
- `phase` is written as `null` on purpose: sketch-bench measures each
  operation alone, outside either phase, so the entry applies to both.
- `provenance` is always written. `CostTableProvenance` gains
  `library_rev`, `producer` and `data_shape`, all optional and recorded, not
  read.

## 6. Algorithm

### 6.1 The exported statistic

The Planner's targets are tail statements: `Pr[err > ε] ≤ δ`, for one value
(`PerValue`) or for every value of one evaluation at once (`WholeResult`).
A seed mean says nothing about the tail, and sketch-bench's 95% two-sided
theory bound is a different confidence level. So sketch-bench exports, per
configuration and per point of its N and m grids:

1. Run `trials` independent trials (fresh seed, fresh data draw from the
   canonical shape).
2. For a per-value entry, record each trial's error for one group. For a
   whole-result entry, record each trial's maximum error over its `G` groups.
3. For each δ in {0.01, 0.05, 0.3}, export the `⌈(1 − δ) · trials⌉`-th
   smallest recorded error.

`error_at[δ]` is then an estimate of the error a fraction δ of evaluations
exceed. The `δ ≥ 3 / trials` rule keeps at least three trials above each
exported quantile.

### 6.2 Lookup

`MeasuredSketchAccuracy::guarantee(readout, δ)` answers one question: is there
a measured error for this configuration at this state size, merge count,
scope and failure probability? It never extrapolates.

1. **Key.** Keep the entries with the same `sketch` (exact parameters),
   `statistic` and `accuracy_metric`. For a whole-result readout, keep the
   whole-result entries with `groups ≥` the readout's `G` (a maximum over more
   groups bounds a maximum over fewer); if none, use the per-value entries and
   the union bound (step 4).
2. **Size axis.** For `Cardinality`, the axis is `distinct_per_state`, taken
   from the source's declared `max_distinct_per_evaluation`; for the
   others, `samples_per_state`. Find the measured values `lo ≤ x ≤ hi` that
   bracket the readout's `x`. If `x` is below the smallest or above the
   largest measured value, or unknown, there is no entry.
3. **Merge axis.** Likewise bracket `m` between measured merge counts. If
   `m` is above the largest measured (16 today), there is no entry.
4. **δ.** Let `δ_needed` be the failure probability this readout must meet
   (6.4). Use the largest grid δ that is `≤ δ_needed`. If none (e.g.
   `δ_needed = 0.004`), there is no entry. For a per-value entry serving a
   whole-result readout, `δ_needed` is first divided by `G`.
5. **Worse neighbour.** The error is the maximum of `error_at[δ]` over the
   up to four corner entries `(lo|hi, m_lo|m_hi)`. All four must exist.
6. **Result.** An `AccuracyGuarantee` with that error as a constant bound, the
   chosen grid δ as a constant failure probability, the entry's metric and
   scope, and provenance `[MeasuredAccuracy { table, library_rev },
   AccuracyTarget { .. }]`.

"No entry" is not an error: the readout keeps its analytical guarantee.

### 6.3 Stage 1 and Stage 2

Nothing changes in Stage 1. Pass 1 still sizes each summary from the
analytical inverses and composes its guarantee with the built-in rules. Stage
1 reads no data workload, so it cannot know a state's size or merge count, and
the measured lookup needs both. Pass 2 still sizes a shared state with
`size_for_all_readers`.

Nothing changes in Stage 2 either, but Stage 2 fixes what the lookup reads:
its materialization and pane choices decide how many rows each state holds
and how many states a readout merges. For a pane chain of pane width `p` read
over a window `w`, `merged_states = w / p`; for one state over the whole
window, 1. `samples_per_state` comes from the source's facts:
`p / sample_interval` rows per series per pane, or the source's share of
`rows_per_second · p` divided by its `series_count` when the interval is not
declared. Unknown facts mean no entry.

### 6.4 Stage 3, step by step

Stage 3 already resizes summaries for the failure probability before it
checks accuracy and prices a candidate. The measured step runs right after
that resize, on the same rebuild path (`with_resized_summaries`), and only
when the table is not empty.

For each candidate:

1. **Failure-probability split (unchanged).** For each query, split its δ
   over its `n` approximate inputs: each input must fail with probability at
   most `δ / n` per value. For a whole-result target the analytical path
   divides further by `G`; the measured path tries the whole-result entry
   first (6.2, step 1) and divides by `G` only when it falls back to
   per-value entries. A state read by several queries keeps the smallest
   share, as today.
2. **Readers.** For each sketch state, collect every reader: the query, its
   local ε (the composed target allocated to this input, as Stage 1 allocated
   it), its metric, scope, `G` and `δ_needed`. A shared state has several.
3. **Measured alternatives.** List the table's configurations of the state's
   family whose lookup (6.2), for **every** reader, gives an error at most that
   reader's ε. Keep only configurations cheaper than the state's current
   (analytically sized, possibly resized) parameters.
4. **Cheapest first.** Order them by the state's own cost under the
   deployment's cost model: memory per state and update CPU per row, from
   `MeasuredOperationCosts` when it has the entry and the formulas otherwise.
5. **Try each.** Rebuild the candidate with the configuration, with the
   readout's local guarantee replaced by the measured one, compose every
   guarantee above it again, and run the normal accuracy check
   (`guarantee_check`) for every reader. Take the first that passes for all
   readers.
6. **Fallback.** If none passes, keep the analytical size. A measured
   configuration never replaces one that is cheaper, and the analytical
   models stay the worst-case fallback for everything the table does not
   cover.
7. **Price and select (unchanged).** Price the rebuilt candidate and compare
   as today. The report shows, per state, the analytical size, the chosen
   size and the guarantee source.

**Sharing.** One shared state must meet every reader. Step 3 tests each
configuration against each reader's own ε, metric, scope and `δ_needed`, so
the shared state is the cheapest measured configuration that is enough for
the tightest reader. If one reader reads a statistic the table does not
cover (a p50 when only p99 was measured, say), no measured configuration
qualifies and the shared state keeps its analytical size. Pass 2's
alternatives (the readers not sharing) remain separate candidates, so Stage 3
still compares a shared measured state with unshared ones.

**Several summaries in one query.** A query `a − b` over two KLL quantiles has
`n = 2` inputs, so each must meet `δ / 2`. At `δ = 0.01` that is 0.005,
below the smallest grid δ, so neither input has an entry and both keep their
analytical sizes. At `δ = 0.1` each needs 0.05, which the grid has. This is
the intended conservative behaviour: the table never spends a failure
probability it did not measure.

### 6.5 The problem example, through the steps

- Stage 1: `kll_k(0.0018)` gives k = 1,564, analytical guarantee 0.0018 at
  δ = 0.01.
- Stage 2: the five-minute window is one state per series per evaluation,
  `merged_states = 1`; at a 1 s sample interval, `samples_per_state = 300`.
- Stage 3, step 1: one input, per value, `δ_needed = 0.01`.
- Step 3: suppose the table holds KLL k ∈ {400, 800, 1600} at
  `samples_per_state` ∈ {100, 1000} and `merged_states = 1`. The lookup
  brackets 300 between 100 and 1000 and takes the worse of the two. If k = 800
  measures at most 0.00176 at both, it qualifies; k = 400 does not.
- Steps 4 to 5: k = 800 is cheaper than k = 1,564; the check passes with the
  measured guarantee, provenance `MeasuredAccuracy`.
- Result: k = 800, about half the KLL state per series. Without the table,
  the selection is k = 1,564, as today.

### 6.6 Cost path

1. sketch-bench measures `rqe_atomic_costs.json` at the executor's
   `asap_sketchlib` rev and records the rev.
2. `convert_rqe_atomic_costs` refuses a rev other than
   `asap_executor::ASAP_SKETCHLIB_REV` (override:
   `--allow-library-rev-mismatch`, which records both revs) and writes the
   object form of 5.2.
3. Plan selection prices from it as today. Until the per-operator-class
   calibration of #791 lands, a sketch-only table must not be used to compare
   exact with sketch plans: `stage_pipeline` and plan selection warn when the
   table has no relational operator (no `Scan`, `Filter`, `TimeRange` or
   `Aggregate` entry), and the validation runs (step 8 of the plan) use the
   measured-cost arms only after that calibration.
4. Long term, sketch-bench's cost numbers become a cross-check of the
   executor-side measurements (open question OQ1).

## 7. Code interface

All new code is in `plan-selection` and `devtools`, plus one variant in
`types` and one constant in `executor`. Names are proposals.

**`crates/plan-selection/src/measured_sketch_accuracy.rs`** (new)

```rust
/// The deployment's measured tail errors of summary configurations: the
/// table Stage 3 reads to choose a measured configuration smaller than the
/// analytical size. Default empty: every summary is sized analytically.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredSketchAccuracy {
    /// Where the numbers come from; required when `entries` is not empty.
    pub provenance: Option<AccuracyTableProvenance>,
    /// One entry per measured (configuration, statistic, scope, size, merge count).
    pub entries: Vec<MeasuredAccuracyEntry>,
}

/// Where a measured-accuracy table's numbers come from. Recorded and carried
/// into each guarantee; the Planner does not compare the rev with its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracyTableProvenance {
    /// The sketch library measured, e.g. `asap_sketchlib`.
    pub library: String,
    /// The library's commit, e.g. the executor's pin.
    pub library_rev: String,
    /// The tool that measured, e.g. `sketch-bench`.
    pub producer: String,
    /// The tool's commit.
    pub producer_commit: String,
    /// The one data shape every entry was measured at.
    pub data_shape: CanonicalDataShape,
}

/// The data shape of the measurements, recorded, not matched (no shape key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalDataShape {
    /// Key distribution, e.g. `{"zipf": {"theta": 1.1, "keys": 10000}}`.
    pub key_distribution: serde_json::Value,
    /// Value distribution, e.g. `{"pareto": {"alpha": 2.0, "scale": 1000.0}}`.
    pub value_distribution: serde_json::Value,
    /// Items per trial.
    pub items: u64,
}

/// One measured configuration at one state size and merge count.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeasuredAccuracyEntry {
    /// The exact parameters measured; only these can be chosen.
    pub sketch: SketchParams,
    /// The statistic read out, e.g. `Quantile { q: 0.99 }`.
    pub statistic: SketchStatistic,
    /// The metric of `error_at`.
    pub accuracy_metric: AccuracyMetric,
    /// One group's value, or the maximum over `groups` groups.
    pub accuracy_scope: AccuracyScope,
    /// Groups per trial for a whole-result entry; 1 for per value.
    pub groups: u64,
    /// Rows inserted into each state.
    pub samples_per_state: u64,
    /// Distinct keys per state, for `Cardinality`; `None` otherwise.
    pub distinct_per_state: Option<u64>,
    /// States merged before the readout; 1 for none.
    pub merged_states: u32,
    /// Independent trials behind each quantile.
    pub trials: u32,
    /// The error exceeded in a fraction δ of trials, per δ.
    pub error_at: Vec<ErrorAtFailureProbability>,
}

/// One point of an entry's tail.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorAtFailureProbability {
    /// δ: 0.01, 0.05 or 0.3, and at least `3 / trials`.
    pub failure_probability: f64,
    /// The empirical `(1 − δ)` quantile of the trial errors.
    pub error: f64,
}

/// One readout of one state, as Stage 3 asks the table about it.
#[derive(Debug, Clone, Copy)]
pub struct SummaryReadout<'a> {
    /// The state's parameters.
    pub sketch: &'a SketchParams,
    /// What is read out of it.
    pub statistic: &'a SketchStatistic,
    /// The reader's target scope.
    pub accuracy_scope: AccuracyScope,
    /// Values per result, `G`, for a whole-result reader; `None` if unbound.
    pub values_per_result: Option<u64>,
    /// Rows per state from the data workload; `None` if unknown.
    pub samples_per_state: Option<u64>,
    /// The source's declared distinct bound, for `Cardinality`.
    pub distinct_per_state: Option<u64>,
    /// States merged per readout, from Stage 2's timing.
    pub merged_states: u32,
}

/// Why a table is refused (bad input).
#[derive(Debug, Clone, PartialEq)]
pub enum MeasuredAccuracyError {
    /// A δ outside {0.01, 0.05, 0.3}, or below `3 / trials`.
    UnsupportedFailureProbability { entry: usize, failure_probability: f64, trials: u32 },
    /// An error negative, non-finite, or smaller at a smaller δ.
    InvalidError { entry: usize },
    /// A family without measured accuracy (only KLL, DDSketch and HLL), or a
    /// metric that does not match it.
    UnsupportedFamily { entry: usize },
    /// Two entries with one key.
    DuplicateEntry { entry: usize },
    /// Entries without provenance, or an empty library rev.
    MissingProvenance,
}

impl MeasuredSketchAccuracy {
    /// Bad input as listed in [`MeasuredAccuracyError`].
    pub fn validate(&self) -> Result<(), MeasuredAccuracyError>;
    /// The measured guarantee of `readout` at failure probability
    /// `failure_probability` (6.2), `None` when no entry applies.
    pub fn guarantee(
        &self,
        readout: &SummaryReadout<'_>,
        failure_probability: f64,
    ) -> Option<AccuracyGuarantee>;
    /// The measured configurations of `family` the table holds for
    /// `statistic`, for Stage 3 to try (6.4, step 3).
    pub fn configurations(
        &self,
        family: SketchAlgorithm,
        statistic: &SketchStatistic,
    ) -> Vec<&SketchParams>;
}

/// The deployment's evidence with the measured-accuracy table in front:
/// summary readouts from the table, everything else from `inner`.
pub struct MeasuredAccuracyEvidence<'a> {
    /// The table, from `PlanSelectionSettings::measured_sketch_accuracy`.
    pub table: &'a MeasuredSketchAccuracy,
    /// The deployment's own provider (`PlanningModels::evidence`).
    pub inner: &'a dyn AccuracyEvidenceProvider,
}

impl AccuracyEvidenceProvider for MeasuredAccuracyEvidence<'_> {
    fn operand_domain(&self, operand: &OperatorNode) -> Option<OperandDomain>; // delegates
    fn output_guarantee(&self, output: &QueryRoot) -> Option<AccuracyGuarantee>; // delegates
    fn summary_guarantee(
        &self,
        readout: &SummaryReadout<'_>,
        failure_probability: f64,
    ) -> Option<AccuracyGuarantee>; // the table, else `inner`
    fn may_provide_evidence(&self) -> bool; // `inner`'s: the table certifies no output without a rule
}
```

**`crates/plan-selection/src/accuracy.rs`** (modified)

```rust
pub trait AccuracyEvidenceProvider {
    /// Unchanged.
    fn operand_domain(&self, _operand: &OperatorNode) -> Option<OperandDomain> { None }
    /// Unchanged.
    fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> { None }
    /// New: a measured local guarantee for one summary readout at failure
    /// probability `failure_probability`, which Stage 3 composes in place of
    /// the analytical one. `None` (the default): the analytical guarantee.
    fn summary_guarantee(
        &self,
        _readout: &SummaryReadout<'_>,
        _failure_probability: f64,
    ) -> Option<AccuracyGuarantee> { None }
    /// Unchanged.
    fn may_provide_evidence(&self) -> bool { true }
}
```

**`crates/plan-selection/src/lib.rs`** (modified)

```rust
pub struct PlanSelectionSettings {
    pub cost_model: CostModel,                               // unchanged
    pub resource_speed: ResourceSpeed,                       // unchanged
    pub non_repeating_query_horizon_seconds: f64,            // unchanged
    pub measured_operation_costs: MeasuredOperationCosts,    // unchanged
    pub ignore_accuracy_guarantees: bool,                    // unchanged
    /// New: measured tail errors; default empty (analytical sizing only).
    pub measured_sketch_accuracy: MeasuredSketchAccuracy,
}
```

`with_resized_summaries` (in `logical-asap-optimization`) takes a resize that
can also return the readout's local guarantee, so the measured guarantee is
composed upward instead of the estimator's:

```rust
/// What a resize gives one state: new parameters, and optionally the local
/// guarantee of its readout (a measured one); `None` keeps the estimator's.
pub struct ResizedSummary {
    pub params: SketchParams,
    pub local_guarantee: Option<AccuracyGuarantee>,
}

pub fn with_resized_summaries(
    roots: &[QueryRoot],
    resize: &dyn Fn(&OperatorNode, &SketchParams) -> Option<ResizedSummary>, // was Option<SketchParams>
) -> Result<Vec<QueryRoot>, SchemaDerivationError>;
```

**`crates/types/src/ir/properties/guarantee.rs`** (modified)

```rust
pub enum GuaranteeSource {
    // ... every existing variant unchanged, except:
    /// The analytical contract a sketch's local guarantee was derived from.
    SketchEvaluation {
        algorithm: String,
        // contract: String,  // removed if OQ2 (a) is accepted
        params: serde_json::Value,
        query: String,
    },
    /// New: the local guarantee came from a measured-accuracy table entry.
    /// Counts as one approximate layer, like `SketchEvaluation`.
    MeasuredAccuracy {
        /// The table: producer and commit, e.g. `sketch-bench@<commit>`.
        table: String,
        /// The library rev the entry was measured on.
        library_rev: String,
    },
}
```

**`crates/plan-selection/src/measured_operation_costs.rs`** (modified)

```rust
pub struct CostTableProvenance {
    pub executor_version: String,              // unchanged
    pub machine: String,                       // unchanged
    /// New: the sketch library rev measured; default `None`.
    #[serde(default)] pub library_rev: Option<String>,
    /// New: the tool and commit that measured; default `None`.
    #[serde(default)] pub producer: Option<String>,
    /// New: the data shape measured at; default `None`.
    #[serde(default)] pub data_shape: Option<String>,
}
```

**`crates/executor/src/lib.rs`** (new constant)

```rust
/// The `asap_sketchlib` commit the executor's kernels are built from; a test
/// checks it against the workspace pin. Converters refuse other revs.
pub const ASAP_SKETCHLIB_REV: &str = "63e565506426020c505ef0b84e0c91f450d09e7c";
```

**`crates/devtools`** (modified and new)

```rust
/// Unchanged name; writes the object form of `MeasuredOperationCosts`.
pub fn convert_rqe_atomic_costs(
    rows: &Value,
    provenance: &SourceProvenance,       // new: rev, producer, machine, shape
    rev_policy: LibraryRevPolicy,        // new
) -> Result<ConvertedCosts, String>;

/// New: sketch-bench's tail-accuracy export to `MeasuredSketchAccuracy`.
pub fn convert_sketch_accuracy(
    rows: &Value,
    rev_policy: LibraryRevPolicy,
) -> Result<MeasuredSketchAccuracy, String>;

/// Whether a library rev other than `ASAP_SKETCHLIB_REV` is refused.
pub enum LibraryRevPolicy {
    /// Default: refuse, naming both revs.
    RequireExecutorRev,
    /// `--allow-library-rev-mismatch`: accept and record both revs.
    AllowMismatch,
}
```

CLI: `stage_pipeline --measured-sketch-accuracy <file>` sets
`PlanSelectionSettings::measured_sketch_accuracy` (also settable in
`--plan-selection-settings`). The selection report prints, per sketch state,
`analytical: Kll{k:1564} -> measured: Kll{k:800} (sketch-bench@<commit>,
asap_sketchlib <rev>)`.

## 8. Decisions

| # | Decision | Why |
| --- | --- | --- |
| D1 | The Planner does not depend on sketch-bench or its `rqe-optimizer` crate. The Planner's devtools own the conversion. | `rqe-optimizer` pulls polars, sketch_oxide and datasketches; sketch-bench already depends on ASAPPlanner, so it would be a cycle; and the Planner must stay deployment-agnostic. |
| D2 | The executor's `asap_sketchlib` pin is authoritative. sketch-bench re-pins to it; the converters refuse a rev mismatch by default. | Numbers measured on another library rev, or another kernel (`FastPath` integer vs. `RegularPath` `f64` Count-Min), describe code the executor does not run. |
| D3 | Measured accuracy may make a summary **smaller**, but only to a configuration actually measured in the table. | The gain is the over-sizing of #798; choosing an unmeasured size would be extrapolation. |
| D4 | The analytical models stay. They are the worst-case fallback whenever no entry applies: outside the measured N or m range, missing family or statistic, shape not covered. Lookup takes the worse neighbour in N and m and never extrapolates. | Keeps today's behaviour exactly whenever the table says nothing. |
| D5 | The exported statistic is the empirical error at a fixed δ ∈ {0.01, 0.05, 0.3}, with the trial count; never a mean. Any δ < 3 / trials is refused. | Targets are tail statements; a mean does not bound a tail, and a quantile from too few trials is noise. |
| D6 | Export both per-value and whole-result (maximum over the measured G groups). Whole-result targets use the whole-result value. | The union bound over G values is what makes whole-result targets expensive; a measured maximum avoids it when it was measured at least at G. |
| D7 | Measured accuracy applies to KLL, DDSketch and HLL. Top-k and Count-Min stay analytical; `precision_at_k` has no Planner metric. | These three have a scalar per-value error the Planner's metrics express. |
| D8 | Canonical shape only, recorded in provenance; no shape key. Revisit once `DataWorkload` declares shape. | `DataWorkload` carries no shape facts, so a shape key could not be matched. |
| D9 | Do not use a sketch-only cost table to compare plans before per-operator-class calibration (#791) lands; warn when a table has no relational operators. | One global calibration factor prices unmeasured relational operators at the sketch's ratio and can flip the choice. |
| D10 | Provenance: add `GuaranteeSource::MeasuredAccuracy { table, library_rev }`. | Reports and tests must tell a measured guarantee from an analytical one. |
| D11 | Stage 1 is unchanged; the measured step runs in Stage 3. | Only Stage 3 binds the data workload, which the lookup needs (rows per state, G). |

## 9. Open questions

### OQ1: the long-term source of cost data

The user wants to discuss this further.

| Option | For | Against |
| --- | --- | --- |
| **(a) Executor-side microbench in ASAPPlanner.** A bench in the executor crate times the kernels the executor runs, and relational operators, and writes `MeasuredOperationCosts` directly. sketch-bench does accuracy only and cross-checks cost. | Same library rev and the same kernel code by construction; covers relational operators (#761, #791); no format conversion; versioned with the executor. | A second benchmark home; idle-machine numbers miss cache and concurrency effects. |
| **(b) sketch-bench for both,** after re-pinning and adding relational-operator benches there. | One benchmark home. | sketch-bench must track the Planner's rev; relational operators would live in a sketch benchmark; the rev check stays a cross-repository contract. |
| **(c) Per-operator profiling of the backend data plane** in real runs. | Real cache, concurrency and handoff effects. | Needs per-operator instrumentation; the numbers depend on the deployment and its load, so they are a deployment's table, not a default. |

**Recommendation: (a) for planning-time costs, plus (c) to validate them.**
Cost must describe the code that runs. The executor is in this repository and
pins the library, so a microbench beside it cannot drift from it, and it is
the natural home for relational operators, which a sketch benchmark should not
own. (a) alone measures each operator on an idle core, so (c) checks, on real
runs, that the predicted per-operator costs and their ranking hold; where they
disagree, the deployment can load its profiled table instead. sketch-bench's
cost rows remain useful as an independent cross-check of the sketch kernels
(the two should agree within a stated tolerance at the same rev).

### OQ2: delete `GuaranteeSource::SketchEvaluation.contract`?

Facts: it is a free string naming the analytical fit, e.g.
`"apache_datasketches_kll_empirical_99_a9b42755072b"`. Non-test code writes
it (`estimators/mod.rs`, `kll.rs`, `hll.rs`, `univmon.rs`); only tests read
it; no decision uses it.

| Option | For | Against |
| --- | --- | --- |
| **(a) Delete it.** `SketchEvaluation` stays the analytical source; measured accuracy goes in the new `MeasuredAccuracy` variant. | Follows the repository rule of deleting fields no non-test code reads; the analytical-vs-measured distinction becomes a typed variant instead of a string convention. | The fit's name leaves the provenance trail (it stays in the estimator's source). |
| **(b) Replace the string with a typed enum** of analytical contracts. | Keeps the name, typed. | Adds a type nothing reads. |

**Recommendation: (a)**, in step 5 together with adding `MeasuredAccuracy`.

## 10. Implementation plan

In order; each step is its own PR.

1. **sketch-bench: re-pin and measure what the executor runs.** Re-pin
   `asap_sketchlib` to the executor's rev, record the rev in every output, and
   also measure the variants the executor actually runs: `RegularPath`
   Count-Min and Count Sketch over `f64`, the heap variants, and UnivMon.
2. **Planner devtools: cost converter.** Emit `memory_bytes_per_state`, exact
   entries and provenance (5.2); add `ASAP_SKETCHLIB_REV` and the rev check.
3. **Planner: per-operator-class calibration (#791).** Plus the warning of D9.
4. **sketch-bench: tail-accuracy export.** Error at δ ∈ {0.01, 0.05, 0.3}
   with trial counts, per value and whole result, over the N and m grids (6.1).
5. **Planner: the measured-accuracy table.** `MeasuredSketchAccuracy`,
   `MeasuredAccuracyEvidence`, `summary_guarantee`, the
   `PlanSelectionSettings` field, `convert_sketch_accuracy`, `stage_pipeline
   --measured-sketch-accuracy`, `GuaranteeSource::MeasuredAccuracy`, and OQ2's
   deletion if accepted. Lookup only; no sizing change yet.
6. **Planner: Stage 3 measured sizing.** Try the table's configurations that
   meet every reader, cheapest first, with the analytical size as the fallback
   (6.4); report the analytical and chosen size per state.
7. **Executor-side cost microbench** (depends on OQ1).
8. **Validation runs (local harness).** Re-plan the 30 ASAPQuery workloads
   three ways: (i) analytical; (ii) measured cost; (iii) measured cost plus
   measured accuracy. Run each selected plan in the three-arm benchmark (the
   backend data plane executing each plan) and compare with measured runtime
   cost and error. Report absolute numbers per arm (CPU seconds, retained
   bytes, cost under cost model 1), the regret of each arm (its measured cost
   minus the lowest measured cost among the plans that met their targets in
   the run), and the violation rate against δ: the fraction of values (per
   value) or of evaluations (whole result) whose measured error exceeds ε.

## 11. Test plan

Each test names its example. Steps refer to section 10.

**Converter (step 2)**

- A KLL k = 200 row with `mem_bytes_per_instance: 9648` gives four entries
  (`SummaryAgg`, `PaneChain`, `SummaryMerge`, `SummaryEstimate`), the first two
  with `memory_bytes_per_state: 9648.0`, and `phase: null`.
- An `exact-sum` row gives entries with `exact: "Sum"`, its estimate as
  `FinalizeExactAccumulator`; `exact-delta-set` and `hydra-hll` stay skipped
  with their reasons.
- A table whose `library_rev` is `0.3.0` is refused with a message naming
  both revs; with `AllowMismatch` it converts and records both.
- The output reads back as `MeasuredOperationCosts` with the same entries.
- `ASAP_SKETCHLIB_REV` equals the rev in the workspace `Cargo.toml`.

**Table validation (step 5)**

- An entry with `trials: 200` and δ = 0.01 is refused (3 / 200 = 0.015).
- An entry whose error at δ = 0.01 is below its error at δ = 0.05 is refused.
- A Count-Min entry is refused (`UnsupportedFamily`); so is a KLL entry in
  `Relative`.
- Two entries with one key are refused; a table with entries and no
  provenance is refused.

**Lookup (step 5)**

- KLL k = 800 measured at `samples_per_state` ∈ {100, 1000} with errors 0.0015
  and 0.00176: a readout at 300 gets 0.00176 (worse neighbour); at 50 or at
  2,000, `None`.
- `merged_states = 20` with merge curves up to 16: `None`.
- `δ_needed = 0.004`: `None`; `δ_needed = 0.07`: the δ = 0.05 value.
- A whole-result readout with G = 50 and an entry at `groups: 100` uses it;
  with only `groups: 20`, it falls back to per-value entries at `δ / 50`,
  which is below the grid, so `None`.
- A p50 readout with only p99 entries: `None`.
- The guarantee's provenance holds `MeasuredAccuracy { table, library_rev }`,
  and `approximate_layer_count` is 1.

**Stage 3 sizing (step 6)**

- The problem example: target rank ε = 0.0018, δ = 0.01, per value; table KLL
  k ∈ {400, 800, 1600}; selected k = 800 with a `MeasuredAccuracy` guarantee.
  With an empty table, k = 1,564 (regression: today's selection unchanged).
- A measured configuration more expensive than the analytical one is never
  chosen: with only k = 1600 measured, the state stays at k = 1,564.
- Sharing: readers A (ε = 0.0018) and B (ε = 0.004) of one state get the
  cheapest configuration meeting A, and the check passes for both. If B reads
  p50 and the table has no p50, the shared state stays analytical.
- Two summaries in `a − b` at δ = 0.01: both stay analytical (0.005 below the
  grid); at δ = 0.1 both may shrink.
- HLL: a cardinality readout without `max_distinct_per_evaluation` stays
  analytical; with one inside the measured distinct range it may shrink.
- A table of any size never changes a candidate that has no sketch state
  (exact plans price and select as before).
- `may_provide_evidence` is unchanged by the table: candidates with no
  composition rule are still discarded early when the deployment's own
  provider gives no evidence.

**End to end (step 8)** The three arms on the 30 workloads, with the report of
section 10, step 8. Acceptance: arm (iii)'s violation rate is at most δ within
sampling error on every workload, and its retained bytes are no larger than
arm (i)'s on any workload.

## 12. Risks and limitations

- **Shape.** Every number is at one canonical shape. Real data with heavier
  tails or more keys per state can have larger errors than measured. The
  Planner cannot detect this until `DataWorkload` declares shape; the
  provenance records the shape so a deployment can judge it, and the
  validation runs measure the violation rate on real workloads.
- **Estimated, not proven, tails.** `error_at[δ]` is an empirical quantile.
  With 1,000 trials the δ = 0.01 point rests on about ten trials; the
  `3 / trials` rule bounds the worst case only loosely. A follow-up may export
  an upper confidence bound of the quantile instead.
- **Monotone neighbours.** The worse-neighbour rule assumes the error between
  two measured sizes does not exceed the larger of the two. KLL's error can
  vary non-monotonically with N near compaction boundaries; dense grids reduce
  the risk.
- **Independence across groups.** A whole-result entry measures G groups from
  one draw; groups in production may be correlated differently.
- **Library drift.** A table measured at another rev is refused by default;
  `--allow-library-rev-mismatch` exists for experiments and records both
  revs. Each pin bump in the executor invalidates the tables.
- **Stale or partial cost tables.** Until #791, a sketch-only table can
  mis-rank exact against sketch plans; D9 warns and the validation waits.
- **Search cost.** Step 6 rebuilds a candidate once per measured configuration
  tried. Configurations are few per family, and trying stops at the first
  that passes, but a workload with many shared states multiplies the work.
- **Failure-probability budget.** Queries with several approximate inputs, or
  whole-result targets falling back to the union bound, often need a δ below
  0.01 and therefore keep the analytical size. That is intended, but it limits
  the gain.
- **Measured guarantees in reports.** A reader may take a `MeasuredAccuracy`
  guarantee for a proof. Reports and docs must label it as measured, at its
  shape and library rev.

## 13. Relation to other designs

- [Accuracy model design](../concepts/accuracy-models.md): this design adds a
  summary-level evidence path under "Evidence and trust boundaries"; the rule
  that a measured maximum is not a failure probability is why the exported
  statistic is a tail quantile with its δ and trial count, never a maximum or
  a mean.
- [Error–Resource Profile](error-resource-profile.md): ERP proposed
  shape-conditioned profiles with multi-fit selection; its Planner consumer
  was removed. This design is narrower: one canonical shape, tail errors at
  three δ, three families, and conversion in devtools.
- [ASAPPlanner layering](planner-layering.md): Stage 1 and Stage 2 are
  unchanged; the new step is part of Stage 3 plan selection.
