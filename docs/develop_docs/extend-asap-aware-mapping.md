# Extend ASAP-aware mapping

Audience: developers adding planner behavior. Read the
[architecture](asap-aware-mapping-architecture.md) and the
[contracts](asap-aware-mapping-contracts.md) first. Each recipe lists the
files to change and the tests to add; keep each change inside the stage that
owns the decision.

## 1. Add a summary family

A new sketch that Pass 1 can offer for an intent.

1. **Type.** Add the variant to `SketchAlgorithm` and its parameters to
   `SketchParams` (`crates/types/src/post_asap/sketch.rs`).
2. **Capability.** Add its row to `FamilyCapability::of`
   (`crates/types/src/post_asap/family_capability.rs`): which inputs it can
   merge (`MergeInputRule`) and which statistics it answers (`StatisticKind`,
   with the merged error). Pass 2's sharing, pane and rollup rules read only
   this table.
3. **Offer and size.** List it for its intents in
   `realization::summary_candidates`, most preferred first, and size it in
   `realization::default_size_params`.
4. **Accuracy.** Add its error model under `accuracy/estimators/` and return
   it from `DefaultAccuracyModel`'s `local_guarantee`, by inverting the same
   sizing formulas. Without a guarantee, every candidate that uses it is
   rejected.
5. **Cost.** Give it a state size and per-operation CPU in
   `stage3_plan_selection.rs` (and accept its parameters in
   `stage3_plan_selection/measured_operation_costs.rs` if deployments measure
   it).
6. **Executor.** Wrap the `asap_sketchlib` implementation in
   `crates/asap-physical-operators/src/summary_kernels/` and register it in
   `factory.rs` and `capability.rs`. Gaps in the library are fixed upstream,
   not with a local copy.
7. **Labels.** Name it in `asap_devtools::candidate_labels`.

Tests: a Pass 1 test in `crates/asap-aware-mapping/tests/per_computation_candidates.rs`
that the intent gets the new candidate (and that a deployment without it
prunes it with `UnsupportedByDeployment`); an estimator test beside the
estimator; an execution test in `crates/integration-tests/tests/` such as
`frequency_execution.rs` or `kll_pane_execution.rs`.

## 2. Add a Pass 2 rule

A new way for computations of one Pass 1 candidate to share, split or reuse
work.

1. In `asap_aware_cse.rs`, add the part it can choose as a `*Candidate` type
   with a finder (for example `rollup_candidates(roots)`), and the function
   that applies a set of them and returns `*Pass2Candidate`s (for example
   `rollup_pass2_candidates`). Read summary families only through
   `FamilyCapability`.
2. In `pass/stage1_to_stage3.rs`, call it from Stage 1 enumeration after the
   existing rules and record what it applied in a new `Pass2Candidate` field.
3. Rebuild its label from that field in `asap_devtools::candidate_labels`.
4. Add its row to the Pass 2 rule table of the
   [design](../design_docs/proposals/planner-layering.md).

A rule must keep every Pass 1 candidate, must not add a family or a readout,
and must keep the composed DAG's meaning. Stage 3 decides whether it pays off.

Tests: unit tests beside the rule in `asap_aware_cse.rs` for when it applies
and when it must not; an end-to-end test like
`crates/integration-tests/tests/reuse_rules.rs` that the planned workload
contains the candidate and that Stage 3 prices it.

## 3. Add a Stage 2 materialization option

1. Add the `Materialization` variant
   (`crates/types/src/post_asap/execution_data_state.rs`) and its timing in
   `crates/types/src/ir/timing.rs`, with its data-state edge rules.
2. Offer it in `stage2_physical_asap_optimization.rs` (the
   `MaterializationCandidate` options), after the existing options so default
   candidate ids stay the same, and gate it on the deployment input that
   allows it.
3. Price it in `stage3_plan_selection.rs`: what it keeps in memory or on
   disk, and what each evaluation rebuilds.

Tests: Stage 2 unit tests for when the option is offered;
`crates/integration-tests/tests/materialization_e2e.rs` and the Example 4
tests in `planner_layering_example4.rs`.

## 4. Add a Stage 3 check or a deployment input

- A new validity check goes in `select_plan` and rejects a candidate with a
  `Rejection { valid: false, reason }`; it never fails the whole selection
  unless the input itself is bad (a new `SelectionError` variant, documented
  as bad input).
- A new pricing input goes in `PlanSelectionSettings` (validated in its
  `validate`); a new statement about the executor goes in
  `DeploymentCapabilities`. Both reach every stage through `PlanningModels`.
- Expose it in `stage_pipeline` and in the
  [Stage Viewer guide](../user_guide_docs/stage-viewer.md).

Tests: a Stage 3 unit test that the check rejects exactly the candidates it
should, and a `crates/devtools/tests/stage_pipeline.rs` test for the flag.

## 5. Add a query-language construct

Lower the construct in its frontend (`crates/frontend-promql/src/promql.rs`,
`crates/frontend-sql/src/sql/`, `crates/frontend-metricsql/src/`) into
existing `NonASAPOp` nodes and `AggIntent`s. If it needs a new intent, add
it in `asap-types` and offer its candidates in Pass 1 (recipe 1). A construct
that cannot be represented is rejected with the frontend's
unsupported-feature error, never lowered approximately.

Tests: frontend tests for the lowering and its rejection cases; the corpus
tests (`analyze_corpora`, `variant_coverage`) show the coverage change.

## Checklist

- [ ] The decision is made in the stage that owns it (contracts §6).
- [ ] Every new public type, field and variant has a doc comment.
- [ ] Unit tests beside the code and one end-to-end test through the stages.
- [ ] The design document's examples still plan the same, or the change is
      explained: run the `planner_layering_example*` tests and
      `stage_pipeline --example planner-layering-1` (and `-2`, `-3a`, `-3b`).
- [ ] The design, developer and user docs that name what changed are updated.
