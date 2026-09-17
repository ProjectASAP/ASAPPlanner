# Extend replacement explanations

Audience: contributors exposing a new optimization in diagnostics or the DAG
viewer. The [reporting reference](replacement-explanations.md) defines the output.

Add the underlying candidate through a
[replacement strategy](add-replacement-strategy.md) first. Explanations consume
search results; they must not rediscover applicability or choose a different
candidate set.

For a new public candidate shape, update `ExplanationKind` and its extraction in
[explanation.rs](../../crates/asap-aware-mapping/src/explanation.rs). Preserve typed
provenance, the original rationale, target identity and location. Update DAG export
and its consumer when the new kind requires presentation changes. A structural
hash narrows lookup; it is not a replacement for checking structural equality.

Use `explain_replacements` for the default registry and
`explain_replacements_with` for an explicit strategy set. Test these public entry
points with accepted and rejected cases, custom models where relevant, and shared
subtrees beneath unshared parents. Confirm that reporting agrees with the actual
searched alternatives and does not turn an unavailable guarantee into an accepted
replacement.
