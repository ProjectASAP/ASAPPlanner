# Design: End-to-End Accuracy Guarantees

## Audience and context

This document is for ASAPPlanner developers, architects, and researchers. It
defines how the planner represents, composes, checks, and explains approximation
guarantees for post-ASAP plans, including nested summaries.

Implementation contracts, sketch formulas, extension steps, and validation
commands live in the
[developer guide](../developer_docs/end-to-end-accuracy-guarantees.md). This
document is the authority for architectural decisions and correctness
invariants; the developer guide is the authority for implementing them.

ASAPPlanner is a mathematical planner. It does not execute sketches or import a
sketch runtime. The planner derives guarantees from committed parameters and
keeps data- or runtime-dependent quantities symbolic. A serving system may
later provide observations that instantiate those symbols, but unavailable
evidence must never be replaced with an optimistic value.

The design has one governing rule:

> Accuracy legality is decided before cost ranking. A cheaper candidate cannot
> override a missing or insufficient guarantee.

## Problem and why now

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

Without an end-to-end model, the planner can select a locally well-sized sketch
whose caller-visible result violates the requested accuracy. Nested summaries,
shared grouping, and cross-query reuse make this a planning concern rather than
an isolated sketch-implementation detail.

## Inputs, outputs, and end-to-end behavior

The observable input is a pre-ASAP query plan whose aggregate intents carry an
`AccuracyTarget`, plus optional statistics and runtime observations. The output
is a candidate space of post-ASAP plans. Each caller-visible approximate value
has a typed `ResultGuarantee`, while rejected candidates carry a reason.

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

## Goals and non-goals

The minimum successful outcome is that no selected approximate result lacks a
machine-readable guarantee that satisfies its accuracy target. The model must
also distinguish incompatible error metrics and preserve the evidence used to
reach its decision.

This design does not execute sketches, import a sketch runtime, assume
statistical independence, or prove arbitrary nonlinear and cross-metric
composition. It does not introduce another correctness policy alongside
`AccuracyTarget`.

## Heilmeier questions

- **What are we trying to do?** Prevent the planner from selecting an
  approximate plan unless it can prove the result meets the caller's accuracy
  requirement.
- **How is it done without this design?** Sketches can be sized locally, but a
  nested plan has no common representation or rule for its combined error.
- **What is new?** Typed guarantee expressions, explicit composition rules,
  budget allocation, and legality filtering before cost ranking.
- **Who cares?** Query authors need accuracy requirements to be meaningful;
  planner and runtime developers need an auditable contract between selected
  parameters and observable results.
- **What are the risks and costs?** Conservative rules can reject useful plans;
  incorrect estimator assumptions can admit unsound plans; symbolic evidence
  increases IR and explanation size.
- **How long will it take?** The core algebra and built-in contracts are one
  planner change. Supplying runtime-dependent TopK and Hydra evidence is a
  separate integration increment.
- **How is success checked?** Unit tests exercise every registered rule and
  rejection boundary; end-to-end tests confirm illegal candidates cannot reach
  cost selection; exported plans expose the proof and rejection reason.

## Required behavior

The design must:

1. Represent each summary's caller-visible correctness requirement with an
   `AccuracyTarget`. The existing `Exact`, `Epsilon`, and `EpsilonDelta`
   variants remain valid where their semantics match, but they are not a
   closed set: add or refine target variants when a new summary requires a
   correctness contract that those variants cannot express faithfully.
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

## Proposed design

### Guarantee IR

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

#### Error metrics

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

#### Symbolic expressions

`BoundExpr` represents constants, sums, products, maxima, and unavailable
statistics. `ProbabilityExpr` represents zero, constants, union bounds, and
unavailable probabilities. Evaluation returns no value when a required leaf
is unknown or malformed.

Numeric leaves must be finite. Bounds must be non-negative, and probabilities
must lie in `[0, 1]`. Invalid values fail closed rather than passing a target
comparison through floating-point behavior.

#### Provenance

`GuaranteeSource` records why a guarantee is believed:

- sketch algorithm and committed parameters;
- child guarantees;
- composition rules;
- accuracy-budget allocations;
- runtime observations, when supplied; and
- required statistics that are currently unavailable.

This information is exported with candidate and rejection data so that a plan
can be audited without reconstructing the proof from planner internals.

### Accuracy-model boundary

Accuracy reasoning and budget allocation are separate from cost modeling.
The accuracy model derives local guarantees, propagates compatible guarantees,
and checks the caller-visible result against its target. The budget allocator
only proposes allocations; it does not prove them. `CostModel` may rank only
candidates that survive the complete accuracy check.

Composition rules are explicit and metric-aware. The built-in model supports
only registered exact, additive, Lipschitz, relative, and exact-aggregation
rules. It uses union bounds rather than assuming independence. Cross-metric and
unregistered approximate compositions are unsupported and fail closed.

Sketch contracts are derived from the parameters committed to the plan and
must identify any estimator-specific premise. Count-Min Sketch and CountSketch
remain distinct L1- and L2-frequency contracts. TopK membership requires a
selection-margin certificate; a point-frequency guarantee is insufficient.
Hydra must include both its inner error and shared-grid collision error.

The concrete interfaces, formulas, evidence fields, and extension procedure
are defined in the
[developer guide](../developer_docs/end-to-end-accuracy-guarantees.md).

### Accuracy targets and allocation

`AccuracyTarget::Exact` accepts only a zero bound and zero failure probability.
`AccuracyTarget::Epsilon` checks the evaluated magnitude. An
`AccuracyTarget::EpsilonDelta` requires both an evaluated magnitude no greater
than epsilon and an evaluated failure probability no greater than delta.

These variants are the currently supported target vocabulary, not a permanent
restriction on summary semantics. A new summary may introduce or motivate a
change to `AccuracyTarget` when its caller-visible requirement is not an
epsilon-style numeric error contract. Any new or revised target must define:

- the result semantics being constrained, including its error metric;
- the evidence and parameters required to prove satisfaction;
- the satisfaction rule and its unknown/unsupported behavior; and
- its serialization, explainability, allocation, and compatibility behavior.

The planner must fail closed until the corresponding guarantee model,
propagation rules, and final satisfaction check exist. It must not force a new
summary into `Exact`, `Epsilon`, or `EpsilonDelta` merely to reuse the existing
API.

The initial allocator uses conservative finite choices, including equal splits
for additive nested layers:

```text
epsilon_i = epsilon_total / approximate_layer_count
delta_i = delta_total / approximate_layer_count
```

Every allocated candidate is resized, propagated, and checked. Allocation does
not constitute proof by itself, and recording a requested delta in metadata is
not evidence that an algorithm achieves it.

### Planner and runtime boundary

The guarantee algebra and parameter-derived contracts live in ASAPPlanner. They
do not require the planner to link to `asap_sketchlib`.

Runtime or planning-time observations are still useful for quantities that are
not fixed by static parameters:

- TopK boundary intervals;
- a concrete stream L2 norm when an absolute CountSketch bound is needed;
- Hydra shared-grid collision statistics; and
- stronger implementation-specific confidence or amplification contracts.

Such evidence must enter through explicit observation/statistics fields and be
recorded in provenance. Its absence leaves expressions symbolic and candidates
unprovable.

### Explainability and export

DAG export includes:

- the selected guarantee and metric;
- symbolic bound and probability expressions;
- guarantee provenance;
- accuracy allocations; and
- rejected candidates with their rejection reasons.

This makes the correctness decision inspectable and ensures the explanation
uses the same candidate space and legality checks as optimization.

## Minimal complexity

Three concepts are necessary:

- `ResultGuarantee` is the authoritative description of caller-visible error;
  reusing `AccuracyTarget` would conflate a request with evidence that the
  request was met.
- `AccuracyModel` separates correctness rules from `CostModel`; embedding
  legality in cost values would let ranking accidentally override correctness.
- `AccuracyBudgetAllocator` separates a proposed per-layer budget from the
  propagated proof; assigning the full target to every layer is unsound.

Symbolic expressions are used instead of a general theorem prover or free-form
text. They are the smallest representation that can preserve unknown
statistics, evaluate supported formulas, serialize the result, and explain why
a candidate was rejected.

## Alternatives and decisions

- **One untyped epsilon:** rejected because rank, cardinality, L1 frequency, L2
  frequency, value, and membership errors are not interchangeable.
- **Put correctness in `CostModel`:** rejected because legality must not depend
  on ranking policy.
- **Treat missing evidence as zero:** rejected because it silently converts an
  unproved candidate into a valid one.
- **Assume independent errors:** rejected; the default uses union bounds.
- **Require a runtime library dependency:** rejected because parameter-derived
  planning contracts and runtime observations have different lifecycles.
- **Copy the inner guarantee onto Hydra:** rejected because it omits shared-grid
  collisions.
- **Use a point-frequency guarantee for TopK:** rejected because it does not
  establish membership at the selection boundary.

## Quality attributes and evidence

- **Maintainability and extensibility:** each new sketch or composition adds one
  local contract and focused tests; unknown variants remain fail-closed through
  non-exhaustive enums.
- **Debuggability and understandability:** DAG export exposes expressions,
  provenance, allocations, and rejection reasons. The observable proxy is that
  a rejected candidate can be diagnosed from exported data without replaying
  cost ranking.
- **Performance and scalability:** expressions are small trees evaluated during
  candidate construction. No numerical performance claim is made; candidate
  count and planning latency should be measured before adding richer allocation
  enumeration.
- **Operability:** runtime-dependent values have named symbolic leaves and an
  explicit observation provenance path.
- **Security and robustness:** malformed non-finite or out-of-range numeric
  leaves fail closed.

## Verification requirements

Tests must cover every registered local contract and composition rule, their
invalid and unknown boundaries, target checking after parameter clamps,
rejection before cost ranking, and exported proof or rejection data. The
detailed test matrix and repository validation commands are maintained in the
[developer guide](../developer_docs/end-to-end-accuracy-guarantees.md#validation).

## Risks, rollout, and exit criteria

The main correctness risk is a mismatch between a planner contract and the
estimator actually selected by a serving implementation. Each parameter-derived
contract must therefore state its estimator premise, and deployments with
different semantics must replace the model rather than reuse the guarantee.

The main availability risk is conservative rejection. TopK and Hydra stay on
the exact/pre-ASAP path until their required evidence is available. This is the
intended rollback behavior: removing or disabling a questionable rule reduces
optimization opportunities without weakening correctness.

The design is ready for use when all workspace tests and warnings-as-errors
checks pass, every selected approximate result has an evaluable satisfying
guarantee, and exported rejection data identifies unavailable evidence. Runtime
observation integration exits its follow-up phase when TopK boundary and Hydra
shared-grid fixtures can be supplied end to end without implicit assumptions.

Advanced nonlinear, induced-norm, interval/Jacobian, and correlation-aware
propagation remains separate work.
