# Add a replacement strategy

Audience: contributors adding a logical optimization or a new replacement source.
Read the [strategy contract](asap-aware-mapping-contracts.md#strategy-contract)
before implementing the rule.

## Define applicability and output

Identify the source shape, the semantic equivalence being claimed, and the
schema, grouping, time, capability, and accuracy preconditions. Implement
`ReplacementStrategy::matches` and `replacements`; return no candidates for a
non-match. Override `propose` when the rule needs to retain typed rejection
reasons, and provide a stable strategy name for diagnostics.

Choose a replacement form deliberately:

- `Summary` for a constructed Post-ASAP result;
- `Rewrite` for a logical alternative that may expose further transformations;
- `ExactComposition` when an exact operation must refer to a child target whose
  alternative remains undecided.

Use [exact_composition.rs](../../crates/asap-aware-mapping/src/exact_composition.rs)
as the example for dependent choices. Use
[rewrite.rs](../../crates/asap-aware-mapping/src/rewrite.rs) for semantic rewrites.
Do not assume `SketchAlgorithmStrategy` only handles aggregates; it also handles
supported binary and temporal-average shapes.

## Construct and register candidates

Reuse the existing realization, schema and guarantee logic. Keep all supported
legal choices, with typed provenance and a concise rationale. Do not parse the
rationale later to recover planner state.

For a deployment-specific rule, pass it in the strategy slice to
`search_workload_with` or `search_workload_with_targets`. For a built-in rule,
update the applicable default registries in
[replacement.rs](../../crates/asap-aware-mapping/src/replacement.rs), including
custom-model and evidence-aware entry points. Workload-dependent rules need
post-CSE context; follow `RollupStrategy`'s integration instead of traversing the
workload independently inside every target invocation.

If preference depends on cost, supply the corresponding model hook as described
in [cost customization](customize-cost-model.md). Sharing uses actual `Rc`
identity; recomputation must not accidentally reuse the same maintained state.

## Verify the behavior

Give each focused test a short statement of the behavior it checks. Cover:

1. A matching shape and unsupported near-matches, including safe direct calls
   to `replacements` without a preceding `matches` call.
2. Semantic and output-schema preservation, grouping/time boundaries, and
   rejection of missing required evidence or capabilities.
3. All supported legal alternatives remaining available under different cost
   preferences.
4. Fixpoint termination, candidate deduplication, and correct shared versus
   independently recomputed identities.
5. Dependent compositions retaining compatible child choices through selection
   and materialization.
6. An end-to-end frontend query, including guarantee/rejection and explanation
   output when affected.

Run focused tests first, then the repository's required formatting, workspace
tests and clippy checks for code changes. Use
[corpus verification](corpus-verification.md) when query coverage is affected.
