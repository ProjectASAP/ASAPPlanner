# End-to-End Accuracy Guarantees Realization Companion

This guide explains how to implement, extend, and validate the end-to-end
accuracy model. Read the
[design document](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees.md)
first. The
design document owns architectural decisions and correctness invariants; this
guide owns concrete interfaces, formulas, evidence requirements, and developer
workflow.

## How one candidate moves through the implementation

The stage pipeline checks accuracy per summary estimate, in Stage 3:

```text
aggregate intent and AccuracyTarget
    -> Stage 1 lists summary implementations, with parameters sized from the
       target (asap_logical_optimizer::pass1::realization::default_size_params)
    -> Stage 2 builds the physical candidates
    -> Stage 3 derives each estimate's local guarantee from the committed
       parameters (AccuracyModel::local_guarantee)
    -> checks it against the query's AccuracyTarget (AccuracyModel::satisfies)
    -> rejects the candidate when there is no model or the guarantee misses
       the target; Count-Min also needs non-negative update weights
    -> prices the remaining candidates (pricing does not prove accuracy)
```

The guarantee is the summary's local one: the stage pipeline does not yet
compose guarantees through nested approximate layers or exact operations over
them. The legacy replacement search did, with the composition rules below; it
was removed, and the coverage it carried is listed in #623. `AccuracyTarget` is
the requirement; `ResultGuarantee` is the evidence produced for one concrete
candidate.

The design document's
[end-to-end example](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees.md#end-to-end-example)
is the numeric walkthrough of the composition design.

The main implementation locations are:

| Concern | Location |
| --- | --- |
| Guarantee and error vocabulary | `asap_types::ir::properties::guarantee` |
| Local guarantees and target check | `asap_logical_optimizer::accuracy` (`local_guarantee`, `satisfies`) |
| Accuracy model Stage 3 calls | `asap_plan_selection::{AccuracyModel, DefaultAccuracyModel}` |
| Parameter sizing | `asap_logical_optimizer::accuracy::estimators` |
| Rejection reasons | `asap_plan_selection::Selection::rejected` |

Read the sections below when changing one of those contracts.

## Realization model

Accuracy reasoning is separate from cost modeling:

```rust
trait AccuracyModel {
    fn local_guarantee(
        &self,
        family: &FieldDataType,
        query: &SketchStatistic,
    ) -> Option<ResultGuarantee>;

    fn satisfies(
        &self,
        guarantee: &ResultGuarantee,
        target: &AccuracyTarget,
    ) -> bool;
}
```

A family with no model (`local_guarantee` returns `None`) is rejected for an
accuracy-targeted query. See the
[workflow design](../design_docs/architecture/input-output-workflow.md#planning-evidence-inputs)
for this boundary.

## Composition contracts

These rules were implemented by the removed legacy search
(`accuracy::composition`); the stage pipeline does not apply them yet (#623).

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
epsilon_rank = 2.296 / k^0.9723
k = ceil((2.296 / requested_epsilon)^(1 / 0.9723))
delta = 0.01
```

A tighter `delta` needs a stronger implementation-specific or amplification
contract and otherwise fails closed. The coefficients are specific to the
cited implementation contract. See
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
only; it does not encode a confidence-level budget. HLL may therefore satisfy
`Epsilon` but cannot satisfy `EpsilonDelta`. Supporting a confidence target
would require an explicitly identified estimator contract and corresponding
parameters whose semantics prove the requested failure probability.

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
through an `AccuracyEvidenceProvider` as typed `PropagationStats` and is
recorded in provenance; the stage pipeline does not read it yet. `NoAccuracyEvidence` is the
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

`Exact`, `Epsilon`, and `EpsilonDelta` are the current vocabulary, not a closed
set. If a new summary has a final-result requirement they cannot express:

1. Define the target's result semantics and compatible `ErrorMetric`.
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
- rejection before Stage 3 prices a candidate; and
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
