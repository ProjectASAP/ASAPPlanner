# Design: End-to-End Accuracy Guarantees

## Audience and context

This document is for ASAPPlanner developers, architects, and researchers. It
defines how the planner represents, composes, checks, and explains approximation
guarantees for post-ASAP plans. The parameter-configuration model applies to
both single-summary and nested-summary plans; nesting is one consumer of the
model, not its scope boundary.

Implementation contracts, sketch formulas, extension steps, and validation
commands live in the
[developer guide](../../developer_docs/end-to-end-accuracy-guarantees.md). This
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

A query may be answered by one approximate summary or by several approximate
operations connected together. For example, an inner summary may estimate a
value for each group, and an outer approximate operation may combine those
estimates:

```text
raw input
    -> inner summary
    -> inner estimate
    -> outer approximate operation
    -> final query result
```

The query supplies an accuracy requirement for its final result. Choosing the
parameters of each summary is called **parameter configuration**. Configuring
each summary independently to meet the entire final-result requirement does not
show that the combined result meets that requirement: both operations can add
error.

For example, suppose the final absolute-error requirement is `0.10`. If the
inner operation may add `0.08` and the outer operation may add another `0.08`,
each operation is individually below `0.10`, but the supported additive rule
only guarantees a combined error of `0.16`. The plan must be rejected or the
two operations must be configured with smaller local error limits, such as
`0.05` each.

Probabilistic guarantees have the same issue. If the inner operation may exceed
its bound with probability `0.01` and the outer operation may also exceed its
bound with probability `0.01`, the planner's conservative union-bound rule says
the combined failure probability may be as high as `0.02`. A final requirement
of `delta = 0.01` therefore cannot assign `0.01` independently to both layers.
The allowed final failure probability is a budget that must cover all failure
events included in the result.

Not every correctness requirement is a numeric error on an estimated value.
For TopK, bounding every estimated frequency does not by itself prove that the
returned set contains the correct keys. The planner also needs evidence that
the confidence interval of the kth selected key is strictly above every
excluded key's interval. Without that separation evidence, TopK membership is
unknown even when individual frequency estimates have small error.

The planner therefore needs one guarantee for the **final query result**: the
value or key set returned after every summary readout and subsequent operation.
For a single summary, that guarantee must be derived from the parameters stored
in the selected plan, not from the parameters originally requested before
rounding, clamping, or empirical adjustment. For a composed plan, the planner
must combine the selected operations' guarantees using an explicit rule.

## Inputs, outputs, and end-to-end behavior

The design has these inputs:

- a pre-ASAP query plan;
- an accuracy requirement attached to an aggregate intent;
- candidate summary implementations and their committed parameters; and
- optional statistics or observations required by a guarantee rule.

It produces post-ASAP candidates. A legal approximate candidate carries a
typed guarantee for its final result. An illegal candidate records why its
guarantee could not be derived or did not meet the requirement. The selected
plan is chosen only from legal candidates.

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

### End-to-end example

Consider two compatible approximate operations and a final requirement
`EpsilonDelta { epsilon: 0.10, delta: 0.02 }`.

1. The allocator proposes local requirements of `(0.05, 0.01)` for each
   approximate operation.
2. Candidate generation chooses concrete parameters for each operation.
3. The accuracy model derives each local guarantee again from those concrete
   parameters. Assume each guarantee evaluates to `(0.05, 0.01)`.
4. The registered additive rule produces a final guarantee of at most
   `(0.10, 0.02)` using addition for the error bounds and a union bound for the
   failure probabilities.
5. The final guarantee satisfies the query requirement, so the cost model may
   rank the candidate.

If either local guarantee is unknown, uses an incompatible metric, or combines
to more than `(0.10, 0.02)`, the candidate is rejected before cost comparison.
The accuracy requirement is the requested contract; the result guarantee is
the planner's evidence that a particular candidate meets it.

## Goals and non-goals

The minimum successful outcome is that no selected approximate result lacks a
machine-readable guarantee that satisfies its accuracy target. The model must
also distinguish incompatible error metrics and preserve the evidence used to
reach its decision.

This design does not execute sketches, import a sketch runtime, assume
statistical independence, or prove arbitrary nonlinear and cross-metric
composition. It does not introduce another correctness policy alongside
`AccuracyTarget`.

## Heilmeier questions used by this design

The schedule-oriented Heilmeier questions are omitted because this document
defines a standing planner contract rather than a time-bounded project.

- **What are we trying to do?** Prevent the planner from selecting an
  approximate plan unless its final result is shown to meet the query's accuracy
  requirement.
- **What is missing without this design?** Individual summaries can be
  configured locally, but a composed plan has no common representation or rule
  for its final error.
- **What is new?** Typed guarantee expressions, explicit composition rules,
  budget allocation, and legality filtering before cost ranking.
- **Who uses the result?** Query authors need accuracy requirements to be
  meaningful; planner and runtime developers need an auditable contract between
  selected parameters and final results.
- **How is success checked?** Unit tests exercise every registered rule and
  rejection boundary; end-to-end tests confirm illegal candidates cannot reach
  cost selection; exported plans expose the guarantee and rejection reason.

## Required behavior

The design must:

1. Represent each summary's final-result correctness requirement with an
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
8. Record whether committed parameters came from a mathematical model,
   empirical input, or a future combination of both, without treating the
   parameter-selection method as correctness evidence by itself.

## Proposed design

### Design overview

The design separates a requested requirement from the evidence produced for a
specific plan:

```text
AccuracyTarget
    describes what the query requires

committed summary parameters + optional evidence
    determine each operation's local ResultGuarantee

CompositionOperator + AccuracyModel
    combine local and child guarantees into the final ResultGuarantee

AccuracyModel::satisfies
    accepts or rejects the candidate before CostModel ranking
```

Four components have distinct responsibilities:

| Component | Responsibility | Does not do |
| --- | --- | --- |
| `AccuracyTarget` | State the required correctness of the final query result | Describe evidence or choose parameters |
| `AccuracyBudgetAllocator` | Propose local requirements for approximate layers | Prove that the combined result is legal |
| `AccuracyModel` | Derive, compose, and check guarantees | Rank candidates by cost |
| `ResultGuarantee` | Record the bound, failure probability, metric, and evidence for one candidate result | Express what the query requested |

Candidate generation first enumerates an implementation, commits its concrete
parameters, and derives a local guarantee from those same parameters. If the
candidate consumes an approximate child, the accuracy model applies the rule
registered for that operator and the involved error metrics. The candidate is
legal only if the resulting final guarantee satisfies the target. Cost ranking
cannot recover a candidate rejected at this stage.

The remaining sections define the guarantee representation, composition rules,
parameter-configuration modes, planner/runtime boundary, and explanation data.

### Guarantee IR

Finalized post-ASAP values may carry a `ResultGuarantee`:

```rust
struct ResultGuarantee {
    metric: ErrorMetric,
    bound: BoundExpr,
    failure_probability: ProbabilityExpr,
    provenance: Vec<GuaranteeSource>,
}
```

Summary state does not itself claim a final-result guarantee. The
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
can be audited without reconstructing the guarantee derivation from planner
internals.

### Accuracy-model boundary

Accuracy reasoning and budget allocation are separate from cost modeling.
The accuracy model derives local guarantees, propagates compatible guarantees,
and checks the final query result against its target. The budget allocator
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
[developer guide](../../developer_docs/end-to-end-accuracy-guarantees.md).

### Parameter-configuration modes

Parameter configuration is a planner-wide concern for every summary candidate,
not a mechanism specific to nested queries. ASAPPlanner must distinguish the
source of a candidate's committed parameters from the guarantee used to prove
that candidate legal.

The design recognizes three modes:

| Mode | Parameter input | Current status |
| --- | --- | --- |
| Mathematical | `AccuracyTarget` plus an algorithm contract, inverted into parameters | Implemented and wired into candidate generation |
| Empirical-input | Observed or estimated workload/data characteristics supplied as planning input | Designed as an extension point; not wired into end-to-end candidate generation yet |
| Combined | Mathematical constraints and empirical input jointly choose parameters | Future work |

#### Mathematical configuration

The mathematical path selects parameters by inverting a registered sketch
contract. For example, an epsilon and delta may determine width and depth. The
planner derives the local guarantee again from the parameters it actually
commits, including clamps, and checks that guarantee against the target. This
is the default path currently available in ASAPPlanner.

#### Empirical-input configuration

The empirical path uses explicit information about the expected input or
workload, such as cardinality, frequency distribution, skew, stream norms, or
observed collision behavior. The evidence may come from a catalog, a prior
measurement, or a runtime-facing integration, but it must enter the planner as
typed input with provenance, freshness, and applicability semantics.

This mode is not wired into the current end-to-end candidate-generation path.
Existing symbolic statistics and posterior-related helpers do not constitute
that integration. Until the planner can carry empirical input through parameter
configuration, guarantee derivation, target checking, and export, the built-in
planner must not claim that an empirical configuration was considered or
selected.

Empirical input can recommend smaller or differently shaped parameters, but a
recommendation is not automatically correctness evidence. If the empirical
method supplies only an expectation or heuristic, legality still requires an
independent guarantee satisfying the target. If it supplies a statistical
certificate, the model must state its population, confidence, validity window,
and failure behavior.

#### Combined configuration

The future combined path may use empirical input to refine a mathematically
safe configuration, select among multiple proven contracts, or allocate an
accuracy budget more efficiently. It must define an explicit composition rule
between mathematical and empirical evidence. The planner must not silently
take the smaller of two bounds, assume independence, or use empirical input to
weaken a mathematical requirement.

All modes converge on the same downstream contract:

```text
parameter inputs
    -> committed summary parameters
    -> guarantee derived for those committed parameters
    -> AccuracyTarget satisfaction
    -> cost ranking
```

The post-ASAP IR and DAG export should identify the configuration mode, input
provenance, committed parameters, resulting guarantee, and any unavailable
evidence. This keeps parameter choice auditable and allows future empirical or
combined implementations without creating a second legality pipeline.

### Accuracy targets and allocation

`AccuracyTarget::Exact` accepts only a zero bound and zero failure probability.
`AccuracyTarget::Epsilon` checks the evaluated magnitude. An
`AccuracyTarget::EpsilonDelta` requires both an evaluated magnitude no greater
than epsilon and an evaluated failure probability no greater than delta.

These variants are the currently supported target vocabulary, not a permanent
restriction on summary semantics. A new summary may introduce or motivate a
change to `AccuracyTarget` when its final-result requirement is not an
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

Every allocated candidate is reconfigured, propagated, and checked. Allocation
is only a proposal, and recording a requested delta in metadata is not evidence
that an algorithm achieves it.

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

- `ResultGuarantee` is the authoritative description of final-result error;
  reusing `AccuracyTarget` would conflate a request with evidence that the
  request was met.
- `AccuracyModel` separates correctness rules from `CostModel`; embedding
  legality in cost values would let ranking accidentally override correctness.
- `AccuracyBudgetAllocator` separates a proposed per-layer budget from the
  propagated guarantee; assigning the full target to every layer is unsound.

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
rejection before cost ranking, and exported guarantee or rejection data. The
detailed test matrix and repository validation commands are maintained in the
[developer guide](../../developer_docs/end-to-end-accuracy-guarantees.md#validation).

## Risks, rollout, and exit criteria

The main correctness risk is a mismatch between a planner contract and the
estimator actually selected by a serving implementation. Each parameter-derived
contract must therefore state its estimator premise, and deployments with
different semantics must replace the model rather than reuse the guarantee.

The main availability risk is conservative rejection. TopK and Hydra stay on
the exact/pre-ASAP path until their required evidence is available. This is the
intended rollback behavior: removing or disabling a questionable rule reduces
optimization opportunities without weakening correctness.

Empirical-input parameterization remains a follow-up until typed empirical
inputs are threaded through candidate parameter configuration, guarantee
derivation, target checking, provenance, and export. Combined parameterization
remains future work until its evidence-composition rule is specified and tested.

The design is ready for use when all workspace tests and warnings-as-errors
checks pass, every selected approximate result has an evaluable satisfying
guarantee, and exported rejection data identifies unavailable evidence. Runtime
observation integration exits its follow-up phase when TopK boundary and Hydra
shared-grid fixtures can be supplied end to end without implicit assumptions.

Advanced nonlinear, induced-norm, interval/Jacobian, and correlation-aware
propagation remains separate work.
