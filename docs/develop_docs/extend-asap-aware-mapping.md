# Extend ASAP-aware mapping

Audience: developers adding planner behavior. Read the
[architecture](asap-aware-mapping-architecture.md) and the
[contracts](asap-aware-mapping-contracts.md) first. Each recipe lists the
files to change and the tests to add; keep each change inside the stage that
owns the decision.

## 1. Add a summary family

A new sketch that Pass 1 can offer for an intent.

1. **Type.** `SketchAlgorithm` is `asap_sketchlib`'s algorithm enum, so the
   algorithm must exist in the library first; its summary type is
   `FieldDataType::Sketch(algorithm)`. Add its parameters to `SketchParams`
   (`crates/types/src/summary_type/parameters.rs`) and its predefined list to
   `parameter_list`.
2. **Capability.** Its row, which inputs it can merge and which statistics
   it answers with the merged error, is the library's family-capability
   table (`SketchAlgorithm::capability`), read unchanged by
   `SummaryTypeCapability::of`
   (`crates/types/src/summary_type/capability.rs`). Pass 2's sharing,
   tumbling window and rollup rules read only this table. Which readouts the executor
   evaluates is its own declaration, `EVALUATED_READOUTS`
   (`crates/executor/src/capability.rs`); add the family's readouts there
   once the executor answers them.
3. **Offer and size.** Place it in `summary_realization::ALGORITHM_PREFERENCE`
   (`crates/stage1-pass1-per-computation-candidates`): `summary_candidates` offers it, in
   that order, for every intent whose statistic its capability row answers.
   To size it, add its list in `crates/types/src/summary_type/parameter_lists.rs` (`parameter_list`).
4. **Accuracy.** Add its error model under
   `crates/accuracy/src/guarantee/` and return
   it from `theoretical_guarantee`, evaluated at the required failure
   probability. Without a guarantee, every candidate that uses it is
   rejected.
5. **Cost.** Give it a state size (the library's `estimated_memory_bytes`
   for the type the executor builds; KLL keeps the planner's own formula)
   and per-operation CPU in `sketch_shape`
   (`crates/cost/src/node/state_bytes.rs`), and accept its parameters in
   `crates/cost/src/measured.rs` if deployments measure it.
6. **Executor.** Wrap the `asap_sketchlib` implementation in
   `crates/executor/src/summary_kernels/` and register it in
   `factory.rs` and `capability.rs`. Gaps in the library are fixed upstream,
   not with a local copy.
7. **Labels.** Name it in `asap_devtools::candidate_labels`.

Tests: a Pass 1 test in `crates/stage1-pass1-per-computation-candidates/src/tests.rs`
that the intent gets the new candidate (and that a deployment without it
is not offered it); an estimator test beside the
estimator; an execution test in `crates/integration-tests/tests/` such as
`frequency_execution.rs` or `kll_tumbling_window_execution.rs`.

## 2. Add a Pass 2 rule

A new way for computations of one Pass 1 candidate to share, split or reuse
work.

1. Add a memo rule under `crates/stage1-pass2-reuse-candidates/src/memo/rules/`
   (for example `rollup.rs` or `topk.rs`): it adds candidates (and, when it
   needs them, groups) to the memo groups it implements, keeping every
   existing candidate. Read summary families only through
   `SummaryTypeCapability`. Shared rewrite helpers live in
   `pass2/asap_aware_cse.rs`.
2. Declare it in `rules.rs` and call it from `generate_logical_asap_memo_candidates`
   (`stage1-pass2-reuse-candidates/src/lib.rs`), after the
   existing rules.
3. Add its row to the Pass 2 rule table of the
   [design](../design_docs/proposals/planner-layering.md).

A rule must keep every Pass 1 candidate, must not add a family or a readout,
and must keep the composed DAG's meaning. Stage 3 decides whether it pays off.

Tests: unit tests beside the rule in `pass2/memo/rules/` for when it applies
and when it must not; an end-to-end test like
`crates/integration-tests/tests/reuse_rules.rs` that the planned workload
contains the candidate and that Stage 3 prices it.

## 3. Add a Stage 2 physical properties option

1. Add the `Lifetime` variant or the valid (timing, lifetime) pair
   (`crates/types/src/stages/physical/properties.rs`, `PhysicalProperties::new` and
   `readable_at`) and its placement in
   `crates/types/src/ir/properties/timing.rs`, with its data-state edge rules.
2. Offer it in `crates/stage2-physical-optimization/src/lib.rs` (the
   `MaterializedOutputCandidate` options), after the existing options so default
   candidate ids stay the same, and gate it on the deployment input that
   allows it.
3. Price it in `crates/cost/src/node/usage.rs` (`asap_cost::node_usage`): what it keeps in memory or on
   disk, and what each evaluation rebuilds.

Tests: Stage 2 unit tests for when the option is offered;
`crates/integration-tests/tests/physical_properties_e2e.rs` and the Example 4
tests in `planner_layering_example4.rs`.

## 4. Add a Stage 3 check or a deployment input

- A new validity check goes in `select_plan` and rejects a candidate with a
  `Rejection { cause, reason }` whose `cause` is a new failed-check
  `RejectionCause` variant (listed in `RejectionCause::failed_a_check`); it never fails the whole selection
  unless the input itself is bad (a new `ErrorSelectingPlan` variant, documented
  as bad input).
- A new pricing input goes in `CostInputs` (validated in its
  `validate`); a new statement about the executor goes in
  `DeploymentCapabilities`. Both reach the stages through `PlanningModels`;
  a stage function takes only the field it reads (Stage 2 takes
  `query_time_results_retained: bool`, not the whole struct).
- Expose it in `asap_planner` and in the
  [Stage Viewer guide](../user_guide_docs/stage-viewer.md).

Tests: a Stage 3 unit test that the check rejects exactly the candidates it
should, and a `crates/devtools/tests/asap_planner.rs` test for the flag.

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
      `asap_planner --workload
      tools/dag-viewer/workloads/planner-layering/example1.json --run-until stage3`
      (and `example2.json`, with `--sql-catalog-table` for each table in
      `tables.json`, `example3a.json` and `example3b.json`).
- [ ] The design, developer and user docs that name what changed are updated.
