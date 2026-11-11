# End-to-End Accuracy Guarantees Realization Companion

This guide explains how to implement, extend, and validate the end-to-end
accuracy model. Read the
[design document](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees.md)
first. The
design document owns architectural decisions and correctness invariants; this
guide owns concrete interfaces, formulas, evidence requirements, and developer
workflow.

## How one candidate moves through the implementation

The implementation follows one candidate from a query requirement to either a
proven Post-ASAP node or a structured rejection:

```text
aggregate intent and AccuracyTarget
    -> enumerate a summary implementation
    -> choose and commit concrete summary parameters
    -> derive the local guarantee from those committed parameters
    -> realize the child and obtain its finalized guarantee
    -> propagate required errors through the operators' error contracts
    -> check the final guarantee against AccuracyTarget
    -> keep legal candidate or record the pruning reason
    -> Stage 3 prices the valid candidates under the cost model (cost does not prove accuracy)
```

For a single summary, the child is exact and propagation leaves the local
summary guarantee as the final guarantee. For nested summaries, child and local
guarantees are combined. `AccuracyTarget` is the requirement; `AccuracyGuarantee`
is the evidence produced for one concrete candidate.

The design document's
[end-to-end example](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees.md#end-to-end-example)
is the canonical numeric walkthrough from allocation through legality
checking.

The main implementation locations are:

| Concern | Location |
| --- | --- |
| Guarantee and error vocabulary | `asap_types::accuracy` |
| Accuracy model and built-in propagation | `asap_accuracy` |
| Candidate construction and legality filtering | `asap_aware_mapping::replacement` |
| Parameter sizing hooks | `asap_aware_mapping::cost_model` |
| Guarantee and rejection export | The `asap_planner` devtool's document (`crates/devtools/src/bin/asap_planner.rs`) |

Read the sections below when changing one of those contracts.

## Realization model

Accuracy reasoning and allocation are separate from cost modeling:

```rust
/// The theoretical error bound of reading `statistic` from a state of
/// `summary_type` with parameters θ at `failure_probability`: the
/// algorithm's error formula. `None` when θ cannot fail that rarely.
pub fn theoretical_guarantee(
    summary_type: &FieldDataType,
    parameters: &SketchParams,
    statistic: &SketchStatistic,
    failure_probability: f64,
) -> Option<AccuracyGuarantee>;

/// The deployment's measured error bound for the same configuration, read
/// first when present; `NoMeasuredGuarantees` has none.
pub trait AccuracyEvidence {
    fn measured_guarantee(
        &self,
        summary_type: &FieldDataType,
        parameters: &SketchParams,
        statistic: &SketchStatistic,
        failure_probability: f64,
    ) -> Option<AccuracyGuarantee>;
}
```

Pass 2 gives each sketch readout the accuracy it is required to meet: a
query's target divided by the propagation factors of the consumers on the path
from the readout to the query root (`accuracy::error_propagation`, a linear
bound per operator and metric, with a validity cap on an operand where the
bound holds only below it, such as the denominator of a ratio). A query that
reads one readout gives it one level; a query that composes several readouts
(a ratio of two quantiles) gives every level of an error and failure-probability
grid not looser than the cap, and each logical edge is then checked against
the levels. A consumer with no contract for a metric gives no edge, so an
approximate value is never treated as exact. Every candidate is
checked against the target before being treated as satisfying it. This is distinct from
candidate visibility: a Stage 1 candidate whose output has no guarantee is
still enumerated and appears in the stage document, but Stage 3 rejects it
for any approximate target. Presence and cost are not accuracy certification.
See the [workflow design](../design_docs/architecture/input-output-workflow.md#planning-evidence-inputs)
for this boundary.

## Error contracts

Only a sketch readout (`SummaryEstimate` over a sketch state) introduces
error. Every other operator passes its inputs' errors on through its error
contract, `asap_accuracy::error_propagation(memo, consumer, metric)`
(`accuracy/src/error_propagation.rs`): a linear bound on the output error
from the input errors in one metric,

```text
error_output <= sum_i factor_i * error_i    while error_i <= validity_cap_i
```

or `None` when the operator has no contract for that metric. A contract
keeps the metric: an input's error and the output's are in the same one.

| Operator | Metric | Factor per input | Validity cap |
| --- | --- | --- | --- |
| `Project` of columns and literals, `Sort`, `TimeRange`, `TimeShift`, `Concat`, a readout, a finalize, `SummaryMerge` | any | 1 | none |
| `Filter` whose predicate reads only columns copied from a source (labels, keys) | any | 1 | none |
| `Project` computing `abs`, `clamp`, `clamp_min`, `clamp_max` or a negation of a column | absolute | 1 | none |
| `Aggregate` (or exact accumulator build) by keys copied from a source, without measure filters or `HAVING`: `count` | any | 0 | none |
| the same: `min`, `max` | absolute, relative | 1 | none |
| the same: `avg` | absolute | 1 | none |
| `+`, `-` | absolute | 1 | none |
| `/` | relative | `1 / (1 - b)` on both | `b` on the denominator |
| `*` | relative | `1 + b` on the left, 1 on the right | `b` on the right operand |

`b` is `RELATIVE_OPERAND_VALIDITY_CAP` (0.1, the weakest error of every
parameter list). Every other operator has no contract: a filter or grouping
on a computed value, a ranking, a `sum`, a comparison, any other function.

The contracts are used in three places:

- **The metric rule** (`asap_accuracy::accepted_metrics`,
  `accuracy/src/metric_rule.rs`): a query root accepts its target's metric
  (every metric when the target names none); an input of a candidate
  accepts the metrics its output accepts that the candidate's contract
  carries. An edge from a readout whose metric its consumer does not
  accept fails (`RootPruneReason::MetricRule`), so an approximate value
  never reaches an operator with no contract for its error. Where no query
  with a target reaches a group, no metric rule applies.
- **Readout requirements** (`asap_accuracy::readout_levels`,
  `accuracy/src/readout_caps.rs`): each query's target is passed from its
  root down to the readouts, divided by each consumer's factor and capped
  by its validity cap, keeping per group the largest cap of each query
  and metric. A query that reads one readout gives it one level, its cap
  at the target's failure probability per value (a whole-result δ split
  over the values one evaluation returns); a query that combines several
  readouts (a ratio of two quantiles) gives every level of the grid
  `MAX_ERROR_LEVELS` × a failure-probability grid that is not looser than
  the cap and the per-value δ. Pass 2 writes each level on a readout
  candidate of its own.
- **$\alpha^{\log}$** (`asap_accuracy::producer_satisfies_logical_requirements`,
  `accuracy/src/logical_edge.rs`): an edge into a readout holds when the
  producer's θ has a guarantee (measured, else `theoretical_guarantee`)
  in the required metric within the required error at the required
  failure probability, and a merged state keeps the guarantee of a build.

A value that is exact has no error and needs no contract. Nothing composes
failure probabilities across readouts beyond the per-value split and the
grid above, and nothing adds a local error at a non-readout operator.

## Built-in sketch contracts

These formulas are planner contracts derived from committed parameters.
Always check the resulting guarantee against the target after applying
parameter clamps.

### KLL

For quantile and rank queries, the built-in contract follows Apache
DataSketches' empirical 99th-percentile single-sided normalized rank-error fit:

```text
epsilon_rank = 2.296 / k^0.9723                      # delta >= 0.01
epsilon_rank = 2.296 / k^0.9723 * delta_scale(delta)  # delta < 0.01
delta_scale(delta) = z(1 - delta/2) / z(1 - 0.01/2)   # standard normal quantiles
k = ceil((2.296 / (requested_epsilon / delta_scale(delta)))^(1 / 0.9723))
```

Below `delta = 0.01` the calibration is scaled by the Gaussian quantile ratio
(`kll::delta_scale`; 1.277 at 0.001, 1.510 at 1e-4). That is a model, not a
measurement, so the guarantee names a separate contract
(`apache_datasketches_kll_empirical_99_delta_scaled_a9b42755072b`). The
coefficients were fitted on Apache DataSketches' C++ KLL, pinned below. The
executor runs `asap_sketchlib`'s own KLL. That library's tests check it
against the DataSketches maximum-rank-error characterization
`2.446 / k^0.9433` at a per-trial failure probability of 0.01, not against this
single-sided fit or its δ scaling.

See
[Apache DataSketches KLL accuracy](https://datasketches.apache.org/docs/KLL/KLLAccuracyAndSize.html)
and its
[C++ contract pinned at `a9b42755072b`](https://github.com/apache/datasketches-cpp/blob/a9b42755072b079fd90b29b9851adc121015c58e/kll/include/kll_sketch.hpp).

### DDSketch

```text
epsilon_relative = alpha
delta = 0
```

### HLL

Generic HLL exposes only its RSE magnitude:

```text
RSE = 1.04 / sqrt(2^p)
epsilon_cardinality = 1.04 / sqrt(2^p)
delta = unknown
```

Sizing inverts the magnitude. HLL's current parameter model contains precision
only; it does not encode a confidence-level budget. Generic HLL may therefore
satisfy a target with only `error` set, but not one that also sets
`failure_probability`.

Classic HLL (its linear-counting branch) has an estimator-specific confidence
bound (`accuracy/guarantee/hll.rs`). Its precision is sized from (ε, δ), and
its failure probability stays symbolic,
`classic_hll_failure_probability[p=P,eps=ε](max_distinct[source])`, until
plan selection binds it with the source's
`DataWorkload.sources[source].max_distinct_per_evaluation`. A missing bound
leaves the target unmet.

### KMV and Theta

```text
RSE <= 1 / sqrt(k - 2)
epsilon_cardinality = 10 / sqrt(max(k - 2, 1))
delta = 0.01
k = ceil(100 / requested_epsilon^2 + 2)
```

This is a conservative Chebyshev 99% contract. A tighter confidence target
fails closed unless another model proves it. The variance premise follows the
[Theta/KMV equations](https://datasketches.apache.org/docs/pdf/ThetaSketchEquations.pdf).

### Count-Min Sketch

CMS uses an L1-normalized one-sided frequency bound:

```text
epsilon_l1 = e / width
delta <= exp(-depth)
absolute_error <= epsilon_l1 * ||f||_1
width = ceil(e / requested_epsilon)
depth = ceil(ln(1 / requested_delta))
```

Posterior CMS relaxation may reduce width only under its documented L1
assumptions.

### CountSketch

CountSketch has a separate L2-normalized point-frequency contract and must not
reuse CMS sizing or posterior relaxation:

```text
epsilon_l2 = sqrt(3 / width)
absolute_error <= epsilon_l2 * ||f||_2
delta <= exp(-depth / 18)
width = ceil(3 / requested_epsilon^2)
depth = an odd integer >= ceil(18 * ln(1 / requested_delta))
```

The failure bound is for the median of independent rows when one row is bad
with probability at most `1/3`. Zero or even depth has no modeled guarantee.

## Runtime or statistics evidence

ASAPPlanner contains the guarantee algebra and parameter-derived contracts.
Planner is not a query executor. Data- or runtime-dependent evidence enters
through `AccuracyEvidence` (measured guarantees) or the data workload's
declared facts, and is recorded in provenance. The default evidence has no measurements. Guarantee derivation remains conservative.

### TopK membership

The membership certificate needs:

- the lower confidence bound of the kth selected item;
- the greatest upper confidence bound among excluded items; and
- the union-bound failure probability of all certificate intervals.

The intervals must already include underlying sketch error. Certify exact
membership only when:

```text
selected_kth_lower_bound > excluded_max_upper_bound
```

The result uses `TopKMembership`, zero membership error, and the supplied
failure probability. Missing, non-finite, invalid, equal, or overlapping
evidence rejects the candidate.

### Hydra shared grid

Compose the inner and shared-grid terms as:

```text
B_hydra = B_inner + B_shared_grid
delta_hydra <= delta_inner + delta_shared_grid
```

When observations are unavailable, preserve
`hydra_shared_grid_collision_bound` and
`hydra_shared_grid_failure_probability` as symbolic leaves and reject an
accuracy-targeted candidate. Never copy the inner guarantee onto Hydra alone.

## Adding or changing an accuracy target

`AccuracyTarget`'s fields (`error`, an optional `failure_probability`, an
optional `accuracy_metric` and an `accuracy_scope`; built with `exact()` or
`max_error(ε)` plus `.failure_probability(δ)`, `.accuracy_metric(m)` and
`.accuracy_scope(s)`) are the current vocabulary, not a closed set. If a new summary has a final-result requirement they cannot express:

1. Define the target's result semantics and compatible `AccuracyMetric`.
2. Define the parameters and evidence needed to prove satisfaction.
3. Add the satisfaction rule, including invalid, unknown, and unsupported
   behavior.
4. Add how the target passes through the error contracts down to the
   readouts; do not invent a split for a target that does not divide.
5. Add serialization, DAG explanation, and compatibility behavior.
6. Add the readout guarantee for summaries that claim the target, and an
   error contract for each operator its error must pass through.
7. Keep candidates fail-closed until the end-to-end guarantee path is implemented
   and tested.

Do not coerce membership, distributional, or another non-epsilon requirement
into an existing variant merely to reuse its API.

## Adding a summary or an error contract

For a new summary, give its readouts a guarantee per listed θ at a failure
probability (`guarantee/`) in one metric, tied to the parameters its list
commits. Record estimator/version premises in provenance. For a new error
contract, add the operator's row to `error_propagation`: the metric it
keeps, the factor per input and any validity cap. An operator without a
row has no contract, so an approximate input never reaches it. Missing
evidence must stay symbolic or produce a structured rejection.

Update DAG export whenever new guarantee or rejection data is introduced.

## Validation

Tests must cover:

- each operator's error contract per metric, and operators without one;
- the metric rule and the readout levels a target gives through the
  contracts;
- malformed and unknown symbolic values;
- target checking after parameter clamps;
- CountSketch L2 sizing distinct from CMS L1 sizing;
- accepted separated and rejected overlapping TopK intervals;
- Hydra inner-plus-shared-grid composition;
- implementation-qualified KLL/HLL/KMV/Theta confidence behavior;
- rejection before cost ranking and global selection; and
- DAG export and frontend-to-post-ASAP integration.

For a new target or summary, add a positive guarantee case and boundary cases for
missing evidence, invalid values, incompatible metrics, and insufficient
parameters.

Run:

```text
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```
