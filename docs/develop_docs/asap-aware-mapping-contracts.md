# ASAP-aware mapping contracts

Audience: developers implementing strategies, models, and consumers of search
results. This page records behavioral contracts; the linked Rust definitions
are authoritative for fields and signatures. See the [library guide](library-api.md)
for complete calling examples and the [code architecture](asap-aware-mapping-architecture.md)
for traversal and registry details.

## Target and candidate identity

`TargetSubDAG` identifies a pre-ASAP `Rc<QueryExpr>` and its structural
`consumer_count`. `new` assumes one consumer; workload search discovers actual
sharing after CSE. Structural equality and shared pointer identity are different:
independent copies can compute equal expressions without sharing maintained state.

`ReplacementSubDAG` packages a replacement, rationale, and typed provenance.
Use the current constructors/fields in [replacement.rs](../../crates/asap-aware-mapping/src/replacement.rs).
Rationale is explanatory text; correctness, identity, and downstream decisions
must use typed data rather than parsing that text.

## Replacement forms

| Variant | Meaning | Consumer obligation |
| --- | --- | --- |
| `Summary` | Constructed Post-ASAP `SummaryNode` DAG | Preserve schema, guarantee, execution-data-state and input contracts |
| `Rewrite` | Alternative Pre-ASAP `QueryExpr` | Allow further discovery; it is not itself a finalized approximate result |
| `ExactComposition` | Exact operation referring to a child target | Select a compatible child and validate the composed result before materialization |

[Exact composition](../../crates/asap-aware-mapping/src/exact_composition.rs)
keeps parent and child choices coupled. Replacing that reference with the first
child candidate during strategy enumeration loses choices and may invalidate
sharing or accuracy reasoning.

## Strategy contract

`ReplacementStrategy` supplies `matches` and `replacements`, with default
`name` and `propose` methods. `propose` can expose structured rejections
as well as accepted candidates.

- Enumerate all alternatives supported by the rule that pass its semantic,
  schema, capability, and accuracy conditions. Do not remove one merely because
  another has a lower cost.
- A preference order supplied by `CostModel` must preserve its candidate set.
- Return an empty result for unsupported shapes; do not assume every target is
  an aggregate or panic on a safe non-match.
- Reuse candidate construction and legality checks instead of maintaining a
  second applicability table for the same rule.
- Leave traversal, canonical sharing, and fixpoint discovery to search.

Exhaustive enumeration is relative to supported rules and available evidence;
it does not promise all theoretically possible plans. Missing accuracy evidence
can make an otherwise applicable sketch ineligible.

## Implementation and summary types

`Implementation` represents an aggregate realization: exact accumulator,
validated sketch, sample, wavelet, statistical model, or `PassThrough`.
`summary_candidates` enumerates algorithm applicability; realization and
accuracy checks determine which candidates can actually be returned.

| Level | Type | Meaning |
| --- | --- | --- |
| Family | `SummaryFamilyType` | Exact state, sketch, sample, wavelet, statistical model, or plain values |
| Sketch category | `SketchCategory` | Kind of question the sketch supports |
| Algorithm | `SketchAlgorithm` | Concrete algorithm such as KLL or DDSketch |
| Committed sketch | `SketchKind` | Validated algorithm/parameter pair with a derived category |

`SketchKind::new` checks the parameter variant. It does not by itself certify
accuracy. Derive the guarantee from committed parameters and the actual readout,
then propagate it and check the result against the target. KLL rank error and
DDSketch relative-value error are different contracts.

## Cost and accuracy

`CostModel::rank_candidates` must return a permutation of the supplied
algorithms. `size_params` proposes parameters; accuracy legality remains the
responsibility of the accuracy model. Cost cannot make a rejected candidate legal.

`estimate_cost` attaches numeric cost to a replacement. Its trait default is
`NaN`, so an integration needing comparable numeric costs must supply a model
that supports them. Structural default costs are not physical measurements.
Deployment evidence must retain its units, scope, provenance, and availability.

Custom intent realization and readout hooks must be implemented together. The
default realization is `PassThrough`; the default extension readout panics if
called without a custom implementation. See [cost customization](customize-cost-model.md)
and the [trait definition](../../crates/asap-aware-mapping/src/cost_model.rs).

The [accuracy implementation companion](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees-developer-guide.md)
explains guarantee formulas, evidence and validation. Runtime-dependent facts
must enter through typed evidence; absence is not proof of zero error.

## Search, ranking and selection

`PlanSpace` stores memo groups by target. Groups retain candidates and structured
rejections. `RankedGroup` presents available candidates in preferred order with
aligned costs; ranking does not discard a candidate. See the Rust definitions
for additional composition and provenance metadata.

`search_workload_with_targets` applies per-root accuracy requirements in addition
to candidate construction checks. Use it when the caller supplies result-level
requirements. `cost_sorted` ranks groups independently; `global_selection`
coordinates choices across groups. Materialization constructs a semantic DAG.
Physical commitment, deployment, and execution remain downstream responsibilities.

## Existing-state matching

`Matcher::is_satisfied_by` is a deployment-provided check between required and
available implementations. There is no default implementation. A common query
category alone does not establish substitutability: check estimator guarantees,
parameters, grouping, data coverage, and the deployment's inventory contracts.

## Explanations

Explanations derive from the searched candidate space and its typed metadata.
Do not duplicate legality in a separate explanation rule. See
[replacement explanations](replacement-explanations.md) for the reporting
contract and [extending explanations](extend-explanations.md) for the workflow.
