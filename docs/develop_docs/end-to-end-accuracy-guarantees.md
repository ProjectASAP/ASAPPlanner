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
proven Post-ASAP node or a structured rejection. The direct DDSketch ratio
exception described below also permits an unproven candidate for backend inspection:

```text
aggregate intent and AccuracyTarget
    -> enumerate a summary implementation
    -> choose and commit concrete summary parameters
    -> derive the local guarantee from those committed parameters
    -> realize the child and obtain its finalized guarantee
    -> propagate guarantees through the registered composition rule
    -> check the final guarantee against AccuracyTarget
    -> keep legal candidate or record AccuracyError
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
| Guarantee and error vocabulary | `asap_types::ir::properties::guarantee` |
| Accuracy model and built-in propagation | `asap_logical_asap_optimization::accuracy` |
| Candidate construction and legality filtering | `asap_aware_mapping::replacement` |
| Parameter sizing hooks | `asap_aware_mapping::cost_model` |
| Guarantee and rejection export | The `stage_pipeline` devtool's document (`crates/devtools/src/bin/stage_pipeline.rs`) |

Read the sections below when changing one of those contracts.

## Realization model

Accuracy reasoning and allocation are separate from cost modeling:

```rust
trait AccuracyModel {
    fn local_guarantee(/* family, query, parameters */)
        -> Option<AccuracyGuarantee>;

    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[AccuracyGuarantee],
        local: Option<&AccuracyGuarantee>,
        stats: &PropagationStats,
    ) -> Result<AccuracyGuarantee, AccuracyError>;

    fn satisfies(
        &self,
        guarantee: &AccuracyGuarantee,
        target: &AccuracyTarget,
    ) -> bool;
}
```

`accuracy::budget::per_input_target` splits a target equally among the
approximate inputs of one composition (ε / n for an additive metric,
(1 + ε)^(1/n) − 1 for a relative one, δ / n). Every candidate must then be resized, propagated, and
checked before being treated as satisfying the target. This is distinct from
candidate visibility: a Stage 1 candidate whose output has no guarantee is
still enumerated and appears in the stage document, but Stage 3 rejects it
for any approximate target. Presence and cost are not accuracy certification.
See the [workflow design](../design_docs/architecture/input-output-workflow.md#planning-evidence-inputs)
for this boundary.

## Composition contracts

### Exact values

An exact value contributes zero error and zero failure probability:

```text
B_exact = 0
delta_exact = 0
```

An exact input does not exempt a local approximate sketch from target checking.

### Additive and Lipschitz composition

Compatible absolute bounds compose without an independence assumption:

```text
B_total <= B_input + B_local
delta_total <= delta_input + delta_local
```

For a registered `L`-Lipschitz transformation:

```text
B_output <= L * B_input + B_local
delta_output <= delta_input + delta_local
```

Failure probabilities use a union bound.

### Relative composition

For a registered multiplicative rule whose values are known to be
non-negative:

```text
epsilon_total =
    epsilon_input
  + epsilon_local
  + epsilon_input * epsilon_local
```

Reject the rule when sign information is missing or values may cross zero.

### Exact aggregation over approximate values

For an exact sum, sum the input bounds and union-bound input failures. If the
number of folded rows is required but unknown, keep the result symbolic.

For exact minimum or maximum, use the maximum input value-error bound and
union-bound failures. This does not prove the identity of a winning key.

### Unsupported composition

Accept approximate-over-approximate composition only when a rule exists for
the operator and metric. Cross-metric and unregistered same-metric composition
must return `UnsupportedComposition`; never treat an approximate child as
exact.

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

A KLL shared by readers with different failure probabilities is sized at the
smallest one. Each reader's `k` is first resized to it at no larger rank
error (`kll::shared_k`), and the largest is kept. See
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
bound (`accuracy/estimators/hll.rs`). Its precision is sized from (ε, δ), and
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
It imports `asap_sketchlib` DDSketch mapping bounds for ratio certification;
see [DDSketch ratio certification](../design_docs/proposals/asap-aware-mapping/ddsketch-quantile-ratios.md). This dependency
does not make Planner a query executor. Data- or runtime-dependent evidence enters
through an `AccuracyEvidenceProvider`, is exposed to propagation as typed
`PropagationStats`, and is recorded in provenance. `NoAccuracyEvidence` is the
default: it supplies no missing facts. Guarantee derivation remains conservative;
the direct DDSketch ratio exception above retains a candidate without claiming
its accuracy is proven.

### TopK membership

`PropagationStats` supplies:

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
4. Add allocation behavior where the target can be divided across layers; do
   not invent allocation for non-divisible semantics.
5. Add serialization, DAG explanation, and compatibility behavior.
6. Add local and composition contracts for summaries that claim the target.
7. Keep candidates fail-closed until the end-to-end guarantee path is implemented
   and tested.

Do not coerce membership, distributional, or another non-epsilon requirement
into an existing variant merely to reuse its API.

## Adding a summary or composition

For a new summary, register its error metric and a local contract tied to the
same parameters that sizing commits. Record estimator/version premises in
provenance. For a new composition, specify compatible input and output metrics,
the bound rule, failure composition, required statistics, and unsupported
cases. Missing evidence must stay symbolic or produce a structured rejection.

Update DAG export whenever new guarantee or rejection data is introduced.

## Validation

Tests must cover:

- exact, additive, relative, Lipschitz, sum, and extremum propagation;
- incompatible metrics and unsupported composition;
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
