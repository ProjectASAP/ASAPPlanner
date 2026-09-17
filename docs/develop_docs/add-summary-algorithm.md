# Add a summary algorithm

Audience: contributors adding a built-in summary algorithm or readout. Algorithm
applicability alone does not make a legal replacement. Read the
[mapping contracts](asap-aware-mapping-contracts.md) and
[accuracy implementation companion](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees-developer-guide.md).

## Define the algorithm and realization

1. Define the supported query intents, input/update semantics, grouping and time
   requirements, state representation, and readout semantics.
2. Extend the relevant types in
   [post_asap](../../crates/types/src/post_asap/mod.rs), including algorithm,
   parameters and readout vocabulary. Preserve validated algorithm/parameter
   pairing, serialization and schema behavior.
3. Add applicability through `summary_candidates` in
   [replacement.rs](../../crates/asap-aware-mapping/src/replacement.rs). Reuse the
   existing summary realization path rather than adding a second dispatch table.
4. Supply parameter sizing and preferences through `CostModel`. Construct the
   actual state input and readout, including any grouping and execution-state
   constraints. A runtime-specific implementation also needs downstream support;
   a logical enum variant is not evidence that a deployment can execute it.

## Establish accuracy before cost ranking

Define the estimator-specific error metric, parameter-derived bound and failure
probability in `AccuracyModel::local_guarantee`. Identify estimator/version
premises. A rank-error contract is not a relative-value contract, and a frequency
bound alone does not certify TopK membership.

Derive the guarantee from the parameters committed to the candidate after
rounding or clamping. For nested summaries, provide compatible composition rules
and budget allocation behavior. Verify the final result against the target,
including per-root requirements supplied to `search_workload_with_targets`.

If proof depends on data or runtime facts, expose them through the typed accuracy
evidence provider and preserve their provenance and validity conditions. Unknown,
invalid, or insufficient evidence must leave the candidate uncertified and keep
the conservative exact alternative available. Do not use an attractive cost,
observed average error, or a default parameter choice as a certificate.

The [UnivMon design](../design_docs/proposals/univmon-frequency-summary.md) and
[its integration tests](../../crates/frontend-promql/tests/univmon_candidates.rs)
illustrate the difference between supported readouts and calibrated guarantees.

## Validate integration

Test supported intents and near-matches, parameter/type validation, readout
schema, and serialization/export. Then exercise:

- a valid guarantee that admits the candidate;
- insufficient parameters after sizing/clamping;
- missing, malformed, or inapplicable evidence;
- incompatible error metrics and unsupported composition;
- end-to-end target rejection before cost ranking;
- exact fallback and structured rejection output;
- shared producer identity with distinct compatible readouts, when applicable.

When costing or physical realization is supported, test its complete evidence
requirements and unavailable cases separately. Run the relevant algorithm and
integration tests, then the repository's required code checks.
