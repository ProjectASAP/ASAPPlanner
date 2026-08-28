# End-to-End Accuracy Guarantees

## Status and scope

This document specifies the accuracy design implemented by PR #303 and tracked
by issue #172. It covers how ASAPPlanner represents, composes, checks, and
explains approximation guarantees for post-ASAP plans, including nested
summaries.

ASAPPlanner is a mathematical planner. It does not execute sketches or import a
sketch runtime. The planner derives guarantees from committed parameters and
keeps data- or runtime-dependent quantities symbolic. A serving system may
later provide observations that instantiate those symbols, but unavailable
evidence must never be replaced with an optimistic value.

The design has one governing rule:

> Accuracy legality is decided before cost ranking. A cheaper candidate cannot
> override a missing or insufficient guarantee.

## Problem

A post-ASAP plan can contain more than one approximate layer:

```text
raw input
    -> inner summary
    -> inner estimate
    -> outer summary or transformation
    -> caller-visible result
```

Sizing every layer independently against the caller's full error target is not
an end-to-end proof. The layers may use different error metrics, their bounds
may compose, and their failure probabilities consume a shared budget. Some
operations, such as TopK selection, need evidence that cannot be expressed by
adding point-estimation epsilons.

ASAPPlanner therefore uses this pipeline:

```text
candidate generation
    -> accuracy-budget allocation
    -> local guarantee derivation
    -> end-to-end guarantee propagation
    -> AccuracyTarget satisfaction
    -> legal candidates only
    -> cost ranking and global selection
```

Rejected candidates remain available in explanatory output with a structured
reason. The planner retains an exact or pre-ASAP fallback when no approximate
candidate can be proved legal.

## Requirements

The design must:

1. Use the existing `AccuracyTarget::{Exact, Epsilon, EpsilonDelta}` as the
   correctness requirement.
2. Preserve the meaning of each error metric instead of treating every bound
   as an interchangeable epsilon.
3. Derive local guarantees from the same committed parameters used to size the
   sketch.
4. Propagate guarantees through supported compositions without assuming
   independence.
5. Check the final guarantee against the target before invoking the cost model.
6. Preserve symbolic statistics, provenance, allocations, and rejection
   reasons in the post-ASAP IR and DAG export.
7. Fail closed for invalid numeric values, unknown required statistics,
   incompatible metrics, and unsupported compositions.

## Guarantee IR

Caller-visible post-ASAP values may carry a `ResultGuarantee`:

```rust
struct ResultGuarantee {
    metric: ErrorMetric,
    bound: BoundExpr,
    failure_probability: ProbabilityExpr,
    provenance: Vec<GuaranteeSource>,
}
```

Summary state does not itself claim a caller-visible result guarantee. The
guarantee is attached to a finalized value, such as a `SummaryEstimate`.

### Error metrics

The built-in model distinguishes:

| Metric | Meaning |
| --- | --- |
| `AbsoluteValue` | Absolute error in the returned value |
| `RelativeValue` | Error relative to the true value magnitude |
| `Rank` | Normalized rank error |
| `Cardinality` | Relative cardinality error |
| `Frequency` | Frequency error normalized by the stream L1 norm |
| `L2Frequency` | Frequency error normalized by the stream L2 norm |
| `TopKMembership` | Correctness of the selected TopK membership set |

These metrics are not implicitly convertible. In particular, Count-Min Sketch
and CountSketch use different frequency norms, and a point-frequency guarantee
does not prove TopK membership.

### Symbolic expressions

`BoundExpr` represents constants, sums, products, maxima, and unavailable
statistics. `ProbabilityExpr` represents zero, constants, union bounds, and
unavailable probabilities. Evaluation returns no value when a required leaf
is unknown or malformed.

Numeric leaves must be finite. Bounds must be non-negative, and probabilities
must lie in `[0, 1]`. Invalid values fail closed rather than passing a target
comparison through floating-point behavior.

### Provenance

`GuaranteeSource` records why a guarantee is believed:

- sketch algorithm and committed parameters;
- child guarantees;
- composition rules;
- accuracy-budget allocations;
- runtime observations, when supplied; and
- required statistics that are currently unavailable.

This information is exported with candidate and rejection data so that a plan
can be audited without reconstructing the proof from planner internals.

## Extension points

Accuracy reasoning and allocation are separate from cost modeling:

```rust
trait AccuracyModel {
    fn local_guarantee(/* family, query, parameters */)
        -> Option<ResultGuarantee>;

    fn propagate(
        &self,
        op: &CompositionOperator,
        inputs: &[ResultGuarantee],
        local: Option<&ResultGuarantee>,
        stats: &PropagationStats,
    ) -> Result<ResultGuarantee, AccuracyError>;

    fn satisfies(
        &self,
        guarantee: &ResultGuarantee,
        target: &AccuracyTarget,
    ) -> bool;
}
```

`AccuracyBudgetAllocator` proposes finite parameter allocations for nested
approximate layers. `CostModel` may rank only the candidates that survive
guarantee propagation and target checking. Deployments may provide stronger
accuracy models, but the default model remains conservative.

## Core composition rules

### Exact values

An exact value contributes zero error and zero failure probability:

```text
B_exact = 0
delta_exact = 0
```

An exact input does not exempt a local approximate sketch from target checking.
This matters when parameter ranges are clamped and the tightest representable
configuration still misses the requested target.

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

The rule is rejected when sign information is missing or values may cross
zero.

### Exact aggregation over approximate values

For an exact sum, input bounds are summed and input failures are union-bounded.
When the number of folded rows is required but unknown, the resulting bound
stays symbolic.

For exact minimum or maximum, the value-error bound is the maximum input bound
and failures are union-bounded. This bounds the returned value; it does not by
itself prove the identity of a winning key.

### Unsupported composition

Approximate-over-approximate composition is accepted only when the model has a
rule for the operator and metric. Cross-metric composition and unregistered
same-metric composition return `UnsupportedComposition`. They never treat an
approximate child as exact.

## Sketch contracts

The formulas below are planner contracts derived from committed sketch
parameters. Parameter clamps are followed by a target check.

### KLL

KLL uses normalized rank error:

```text
epsilon_rank = 2 / k
k = ceil(2 / requested_epsilon)
```

The built-in contract has failure probability `0.01` (99% confidence). A target
with a tighter `delta` requires a stronger deployment-specific model or an
amplification contract and otherwise fails closed.

### DDSketch

DDSketch uses its committed deterministic relative-error parameter:

```text
epsilon_relative = alpha
delta = 0
```

### HLL

HLL starts from relative standard error:

```text
RSE = 1.04 / sqrt(2^p)
```

The default model uses a conservative Chebyshev 99% interval:

```text
epsilon_cardinality = 10 * 1.04 / sqrt(2^p)
delta = 0.01
```

Sizing inverts this bound. If the supported precision cap cannot satisfy the
target, HLL is rejected even when it would be the cheaper or preferred family.

### KMV and Theta

The default model uses:

```text
RSE <= 1 / sqrt(k - 2)
epsilon_cardinality = 10 / sqrt(max(k - 2, 1))
delta = 0.01
k = ceil(100 / requested_epsilon^2 + 2)
```

This is also a conservative Chebyshev 99% contract. Tighter confidence fails
closed unless another model proves it.

### Count-Min Sketch

CMS uses an L1-normalized one-sided frequency bound:

```text
epsilon_l1 = e / width
delta <= exp(-depth)
absolute_error <= epsilon_l1 * ||f||_1
```

Sizing uses:

```text
width = ceil(e / requested_epsilon)
depth = ceil(ln(1 / requested_delta))
```

Posterior CMS relaxation may reduce its width only under its documented L1
assumptions.

### CountSketch

CountSketch has a separate L2-normalized point-frequency contract. It does not
reuse CMS sizing or posterior relaxation:

```text
epsilon_l2 = sqrt(3 / width)
absolute_error <= epsilon_l2 * ||f||_2
```

One row is bad with probability at most `1/3`. For an independent odd number of
rows, the median is assigned the conservative Hoeffding bound:

```text
delta <= exp(-depth / 18)
width = ceil(3 / requested_epsilon^2)
depth = an odd integer >= ceil(18 * ln(1 / requested_delta))
```

Zero or even depth has no modeled guarantee. Width/depth caps are checked
against the target after sizing.

## TopK membership certificate

A heap attached to CMS or CountSketch can provide point-frequency estimates,
but those estimates do not prove that the returned keys are the true TopK set.

`PropagationStats` therefore accepts three pieces of evidence:

- the lower confidence bound of the kth selected item;
- the greatest upper confidence bound among excluded items; and
- the union-bound failure probability of all intervals used by the
  certificate.

The intervals must already be widened by the underlying sketch error. Exact
membership is certified only when:

```text
selected_kth_lower_bound > excluded_max_upper_bound
```

The result then has metric `TopKMembership`, zero membership error, and the
provided interval failure probability. Missing values, non-finite values,
invalid probabilities, equality, or overlap reject the candidate.

Static planning currently has no ordinary source for these boundary intervals,
so TopK sketch candidates remain fail-closed until planning-time or runtime
evidence is supplied.

## Hydra shared-grid composition

A Hydra result includes both the inner per-subpopulation sketch error and an
outer shared-grid collision term:

```text
B_hydra = B_inner + B_shared_grid
delta_hydra <= delta_inner + delta_shared_grid
```

The second expression is a union bound. The shared-grid collision bound and
failure probability depend on deployment/data statistics, so the default
planner records them as:

```text
hydra_shared_grid_collision_bound
hydra_shared_grid_failure_probability
```

The resulting expression and provenance are visible in the IR. Because these
leaves cannot yet be evaluated, an accuracy-targeted Hydra candidate is not
admitted. Copying the inner guarantee onto Hydra without the shared term would
be unsound.

## Accuracy targets and allocation

`AccuracyTarget::Exact` accepts only a zero bound and zero failure probability.
`AccuracyTarget::Epsilon` checks the evaluated magnitude. An
`AccuracyTarget::EpsilonDelta` requires both an evaluated magnitude no greater
than epsilon and an evaluated failure probability no greater than delta.

The initial allocator uses conservative finite choices, including equal splits
for additive nested layers:

```text
epsilon_i = epsilon_total / approximate_layer_count
delta_i = delta_total / approximate_layer_count
```

Every allocated candidate is resized, propagated, and checked. Allocation does
not constitute proof by itself, and recording a requested delta in metadata is
not evidence that an algorithm achieves it.

## Planner and runtime boundary

PR #303 implements the guarantee algebra and the parameter-derived contracts in
ASAPPlanner. It does not require ASAPPlanner to link to `asap_sketchlib`.

Runtime or planning-time observations are still useful for quantities that are
not fixed by static parameters:

- TopK boundary intervals;
- a concrete stream L2 norm when an absolute CountSketch bound is needed;
- Hydra shared-grid collision statistics; and
- stronger implementation-specific confidence or amplification contracts.

Such evidence must enter through explicit observation/statistics fields and be
recorded in provenance. Its absence leaves expressions symbolic and candidates
unprovable.

## Explainability and export

DAG export includes:

- the selected guarantee and metric;
- symbolic bound and probability expressions;
- guarantee provenance;
- accuracy allocations; and
- rejected candidates with their rejection reasons.

This makes the correctness decision inspectable and ensures the explanation
uses the same candidate space and legality checks as optimization.

## Validation strategy

Tests cover:

- exact, additive, relative, Lipschitz, sum, and extremum propagation;
- incompatible metrics and unsupported composition;
- malformed and unknown symbolic values;
- target checking after parameter clamps;
- CountSketch L2 sizing distinct from CMS L1 sizing;
- accepted separated and rejected overlapping TopK intervals;
- Hydra inner-plus-shared-grid symbolic composition;
- KLL/HLL/KMV/Theta confidence and `EpsilonDelta` behavior;
- rejection before cost ranking and global selection; and
- DAG export and frontend-to-post-ASAP integration.

The repository validation commands are:

```text
cargo fmt --all
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

## Non-goals

- Adding another correctness-policy enum alongside `AccuracyTarget`.
- Assuming statistical independence by default.
- Treating all sketch errors as a common epsilon.
- Claiming arbitrary nonlinear or cross-metric composition.
- Executing sketches or importing a sketch runtime into ASAPPlanner.
- Treating unavailable runtime evidence as zero.

Advanced nonlinear, induced-norm, interval/Jacobian, and correlation-aware
propagation belongs in separate follow-up work.
