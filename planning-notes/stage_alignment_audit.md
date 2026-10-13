# Stage/term/architecture alignment audit — ASAPPlanner

Date: 2026-10-13. Read-only; every read was through git objects in `/mydata/ASAPPlanner-restack646-v5` after `git fetch origin`.

References:
- `docs/design_docs/proposals/planner-layering.md` @ origin/main (2a075a64) — "the doc"
- `decoupling_op_and_expr.md` (#511) and `asap-primitive-schema.md` (#573) @ origin/main

Code audited:
- (a) origin/main 2a075a64 (#542, #665, #666, #543, #667 merged)
- (b) #561 local commit d4483945, which sits on d3e39fe4 (the PR head) and is not pushed. Its PR title is still "enumerate local logical alternatives without execution timing".
- (c) #575 origin/stack/509-c2-stage1-workload @ ec00b3c6, based on old #561 d3e39fe4. It still uses the old #561 names; those are noted below but not counted as new findings.
- #576 (origin/stack/509-c3-stage2-3 @ bd70f05f) was only skimmed. It sits on a pre-restack base and uses names older than #575's.

Legend: **A** = aligned, **M** = misaligned, **–** = missing (no code concept). `[judgment]` marks a call that is mine, not something the doc settles.

---

## 1. Term table

### Stages, passes, candidate sets

| Doc term | Code name(s) (file:line) | Status |
|---|---|---|
| Stage 0 "Language-specific frontends" | crates `frontend-{sql,promql,metricsql,common}`; `lower_promql_query_workload`, `lower_sql_dialect`, `lower_metricsql_query_workload`; facade `planner/src/lib.rs:203 lower()` | A (crate names fit; the facade doc says "lowering", which is fine) |
| Stage 0 output `CandidateLogicalDAGs` (set of `LogicalDAG`) | No type. Frontends return `QueryRoot` per query (`types/src/ir/query.rs:7`), wrapped in `ParsedWorkload` (`types/src/parsed_workload.rs:32`). The workload DAG is `Vec<(Id, QueryRoot)>` | – (no `LogicalDAG` / `CandidateLogicalDAGs` name anywhere: 0 hits on all three refs) |
| Stage 1 "Logical ASAP-aware optimization" | main: no Stage 1 module. The legacy search (`replacement.rs`, `pass/major.rs`) mixes Stage 1 and Stage 3. #561: `per_computation_candidates.rs` (module doc says "Stage 1 … Pass 1"). #575: `logical_candidates.rs::compose_logical_candidate` ("#509 Stage 1") | M on main; A in #561 |
| Pass 1 "Per-computation candidate generation" | #561 `generate_per_computation_candidates` (`per_computation_candidates.rs:275`), `PerComputationCandidates` (:149). #575 still has `enumerate_local_logical_candidates` / `LocalLogicalCandidates` (old names) | A in #561 |
| "computation" / "eligible sub-DAG" / target | #561 `ComputationTarget` (:134) = one measure of an eligible `Aggregate`; eligibility in `generate_per_computation_candidates` (:268–:275). Legacy: `TargetSubDAG` (`replacement.rs:450`), `bindable_intent` (:2574) | A in #561. Legacy `TargetSubDAG` is a different concept (any bindable site) — M |
| Per-computation options, `Replacements(s)` | #561 `ComputationChoices { candidates, pruned }` (:72), `ComputationCandidate {Exact, ExactAccumulator, Summary, AvgAsSumCount}` (:46) | A. "Choices" is a fine name for `Replacements(s)` `[judgment]` |
| PRUNE(c, reason) | #561 `PruneReason {RequiresExact, NullableAvgInput}` (:63), stored in `ComputationChoices.pruned`. Legacy: `RejectedCandidate` (`replacement.rs:619`), `TargetSubDAGCandidates.rejected` | A in #561. Legacy says "rejected", while the doc keeps "reject" for Stage 3 — M |
| "Summary replacement" (Pass 1 decision) / rewrite rules R / `r.APPLY` | Legacy: `ReplacementStrategy` (`replacement.rs:653`), `ReplacementSubDAG` (:521), `Replacement` (:486), `ReplacementProvenance` (:565); `rewrite.rs` `AvgToSumOverCountStrategy`. #561 has no rule/template abstraction: candidates come from `computation_candidates_for_intent` (:179) plus `replacement::summary_candidates` | Legacy "replacement" is the right word, but the legacy type is a ranked, sized, built sub-DAG (Stage 1 and Stage 3 mixed) — M. #561 matches Algorithm 1 but has no explicit rule/template object (– ; acceptable for now `[judgment]`) |
| Pass 1 SUBSTITUTE / Cartesian product → whole-workload candidates | #575 `compose_logical_candidate(inventory, choice)` (`logical_candidates.rs:399`), with internal `realize`, `realize_measure`, `realized` | A in substance. The internal names use "realize" where the doc says SUBSTITUTE — M (minor) `[judgment]` |
| Pass 2 "ASAP-aware CSE" | main: no Pass 2 stage. Pieces are scattered: identical-expression rule = `types/src/ir/cse.rs:270 share_common_sub_dags` (run before the legacy search and again after selection in `MajorPass`) and `replacement::SharedSubDAGStrategy` (:3927); summary-capability sizing = `accuracy/reconciliation.rs` (`AccuracyReconciliationStrategy`); window composition = `pane_sharing.rs`, `recurrence.rs`, `post_asap/summary_window.rs`; `rollup.rs`, `topk_reuse.rs`, `grouping.rs` (Hydra) | M: no Pass 2 boundary, and CSE runs *before* Pass 1 in the legacy flow |
| Stage 1 output `CandidateLogicalASAPDAGs` (set of `LogicalASAPDAG`) | **Name collision:** `replacement::CandidateLogicalASAPDAGs<Id>` (`replacement.rs:4134`) is the legacy per-target inventory (`TargetSubDAGCandidates` groups). It carries Stage 3 methods `cost_sorted` (:4447) and `global_selection(cost_model)` (:5676). #575 returns one candidate as `Vec<(Id, QueryRoot)>` with no set type | M (the doc's name sits on the wrong concept); `LogicalASAPDAG` – |
| Stage 2 "Physical ASAP-aware optimization" | main: no stage function. #576 adds `physical_candidates::stage2_physical` and `PhysicalCandidate` (MVP: no materialization choice) | – on main |
| Materialization options `NotMaterialized` / `MatAtIngestion(medium)` / `MatAtQuery(medium)` | `Materialization` (`types/src/post_asap/execution_data_state.rs:48`), `Medium {Memory, Disk}` (:37) | A (#667) |
| Materialization assignment `m` | `MaterializationAssignment` (`types/src/ir/timing.rs:76`), `apply_materialization_timings` (:225), `OperatorNode.materialization` (`ir/node.rs:99`) | A |
| PRUNE "upstream of ingestion-time node not at ingestion time" | `ExecutionDataStateError::UpstreamNotAtIngestion` (`timing.rs` resolve) | A |
| Retention ("how long") | none | – (doc TODO) |
| Physical operator implementation | Executor side: `asap-physical-operators/src/physical_planner/*`. #576 `implement()` rewrites exact TopK to sort+limit | – in the planner on main |
| Stage 2 output `CandidatePhysicalASAPDAGs` | none (0 hits). #576 has `Vec<PhysicalCandidate>` | – |
| `PhysicalASAPDAG` | `types/src/ir/physical_export.rs:94`; `compile_physical_asap_workload` (:423); `PlanOutput::execution_timed_dag` (`pass/mod.rs:232`) | A for the type. The method name `execution_timed_dag` is M `[judgment]`: it returns the selected `PhysicalASAPDAG`; suggest `physical_asap_dag` |
| Stage 3 "Plan selection" | main: `CandidateLogicalASAPDAGs::global_selection`, `GlobalSelection` (`replacement.rs:5124`), `TargetSubDAGSelection` (:5070), `cost_sorted`, `rank_group`, `cse_share_decision` (cost_model.rs). #576: `plan_selection::stage3_select` | M on main (selection lives on a Stage 1 type in the Stage 1 module). #576 module name `plan_selection` is A |
| Cost model / accuracy model | `CostModel` (`cost_model.rs:386`), `AccuracyModel` (`accuracy/mod.rs:36`), `PlanningModels` (`pass/mod.rs:47`), empirical: `erp.rs`, `empirical_cost.rs`, `empirical_comparison.rs` | Terms A; placement M (see §3) |
| Stage 3 output "one selected PhysicalASAPDAG" | `PlanOutput` (`pass/mod.rs:138`) holds selected `QueryPlan`s; the DAG is exported by `execution_timed_dag` | partially A |
| Stage 4 "Execution (deployment)" | crate `asap-physical-operators` (runtime, physical_planner, summary_kernels) | A in role; crate name ≠ `executor` (#572) |
| Facade | `planner::e2e_plan` runs `MajorPass` (`planner/src/lib.rs:169`); `OptimizationPass`, `PassRegistry` | M: the doc has no "optimization pass" or "major pass". #572 Q36 retired `MajorPass` and the facade should run Stage 1→2→3. `let fallback = MajorPass` (lib.rs:175) is a non-execution "fallback" |

### IR terms (#511 / #573)

| Doc term | Code | Status |
|---|---|---|
| `OperatorNode`, `Operator` | `ir/node.rs:99`, `:34` | A. It has a `materialization` field that #573 §6.1 does not show (doc gap) |
| `NonASAPOp`, `ASAPOp` | `ir/non_asap.rs:57`, `ir/asap.rs:22` | A |
| `ScalarExpr`, `Predicate`, `ProjectItem`, `SortKey` | `ir/scalar.rs:34` … | A |
| `SummaryAgg`, `SummaryEstimate`, `FinalizeExactAccumulator`, `MaintainPopulation`, `EvaluatePopulation`, `SummaryMerge` | `ir/asap.rs:25,38,44,48,53,58` | Names A. Doc comments M: `SummaryEstimate` says "Read out a query result…" (asap.rs:36) and `FinalizeExactAccumulator` says "the maintenance-to-read boundary" (asap.rs:42) |
| "evaluation", not "readout" | `SketchStatistic` (`post_asap/sketch.rs:596`), `PopulationStatistic`, `EvaluatePopulation.evaluation` | Names A. "read out" is left in comments: `sketch.rs:536` "Sketch read-out queries", `pre_asap/schema.rs:135`, `ir/scalar.rs:340`, `dag_export.rs:1125`, `query_time/mod.rs:18`, `cost_model.rs:520`, `asap-aware-mapping/src/lib.rs:18`, `physical_planner/mod.rs:411`. **#573 itself still says "read out" (lines 85, 252, 866, 952), so the doc needs the same fix** |
| `MaintainedPopulation` / "maintained membership" | `post_asap/maintained_population.rs:51`; `asap-aware-mapping/src/maintained_population.rs` `MaintainedPopulationStrategy` | A — "maintain" is a #573 term *for populations*. Do not rename these |
| Coverage = definition + selection | `SummaryCoverage` (`ir/summary_coverage.rs:28`), `SelectionBox`, `SummaryCoverage::derive` (:82), `OperatorNode::coverage()` (derived) | A (#646) |
| `ExecutionTiming {IngestionTime, QueryTime}` | `post_asap/execution_data_state.rs:20`, derived from `Materialization::timing()` | A (#573 §6.1 uses it; derived from materialization as the doc requires) |
| `ResultGuarantee` | `post_asap/guarantee.rs:379` | A |

---

## 2. Leftover non-doc vocabulary

Counts are **lines** matching the term (case-insensitive) in `crates/**/*.rs` on origin/main, excluding the vendored MetricsQL parser. Tests are included. `local`, `phase` and `replacement` are noisy (thread-local, `boxed_local`, pane phase, `replacement::` paths); the main identifiers are the useful part.

| Term | Lines by crate (main) | Main identifiers / where | Verdict |
|---|---|---|---|
| `maintenance` | asap-aware-mapping 134, types 26, integration-tests 20, asap-physical-operators 9 | `cost_model.rs`: `maintenance_operation_plan_cost_rate` (:232), `default_cse_shared_maintenance_cost` (:364), `CostInputs.summary_maintenance_cost_per_update` (:158), trait method `maintenance_cost_per_update` (:619). `types/post_asap/execution_data_state.rs`: `EvaluationUnderMaintenance` (:130), `InvalidMaintenanceBinary` (:180). `ir/physical_export.rs:456` `maintenance_dependency`; `ir/timing.rs` docs (:22, :45, :799, :912, :1099); `replacement.rs:2908 maintenance_exact_values`; `asap.rs:42`; `physical_planner/candidates.rs:1,15,203`, `mod.rs:55` ("maintenance search/planning") | M wherever it means MatAtIngestion or "kept state". In cost names it means per-update summary build cost `[judgment]`: rename to `ingestion`/`update`/`build` terms |
| `maintained` (non-population) | asap-aware-mapping 132, types 26, asap-physical-operators 20, integration-tests 15, frontend-sql 13 | `recurrence.rs:414 maintained_cost_rate`, `replacement.rs:5507 maintained_summary`, test `continuously_maintained_dag`, "maintained precompute" (`promql_rows.rs:93,210`), "when the state is maintained" (`timing.rs:45`) | M where it means materialized; A for `MaintainedPopulation*` |
| `lifecycle` | types 2, integration-tests 1 | Mostly removed by #667. Left: `integration-tests/tests/materialization_e2e.rs:4` (historical note), `operator_properties.rs:182` (unrelated "DC lifecycle"), `workload.rs:562` (unrelated) | essentially A |
| `readout` / `read out` | types 6, asap-aware-mapping 2, asap-physical-operators 1 | listed in §1 | M (comments only) |
| `realiz*` | asap-aware-mapping 482, integration-tests 27, types 12, devtools 7 | `replacement::Realization` (:713), `RealizationError` (:400), `realizations_for_intent` (:874), `realize_child` (:1909), `realize_child_with` (:2029), `ReplacementProvenance::SummaryRealization`, `RootPhysicalRealization`, `PhysicalSummaryInputRuleResult::Realized`, `OptimizeError::Realization` (`pass/mod.rs`); crate lib.rs terminology table row "Realization: Enumerate ranked physical forms" | M `[judgment]`. The doc uses "realization" once, for "intent has a realization in F" (eligibility). The legacy "Realization" means a *ranked, sized* choice, which the doc splits into Pass 1 candidate (unsized) + Stage 3 selection |
| `fallback` (non-execution) | frontend-common 28, asap-aware-mapping 49, frontend-sql 7, devtools 6, integration-tests 6, types 8, planner 2 | `planner/src/lib.rs:175 let fallback = MajorPass`; `replacement.rs:1887–1891` "rank-and-take-first, and its fallback"; `recurrence.rs` "structural fallback" (:341–:534); `cost_model.rs:244` "raw/pre-ASAP fallback baseline"; `CompositionDecision` doc "raw fallback" (:5099). Frontend uses are parser/name-resolution fallbacks (fine) | M in planner crates (memory: rename fallback-named items at next restack) |
| `alternative(s)` | asap-aware-mapping 66, devtools 9, asap-physical-operators 8, types 4 | `lib.rs:34` "propose local alternatives"; `TargetSubDAGCandidates.candidates` doc "every distinct alternative"; tests `repeated_roots_do_not_duplicate_alternatives`, `profiles_for_alternatives` | M (doc says candidates/options) |
| `local` (planning sense) | asap-aware-mapping ~40 of 147 | `AccuracyModel::local_guarantee` (`accuracy/mod.rs:51`; impls :97, `estimators/mod.rs:58,196`), `apply_local_rules`, `local_target`, `local_metric`, lib.rs "local replacements/alternatives" (`replacement.rs` module doc ~:50–:56) | M `[judgment]` for `local_guarantee`: the doc has no "local"; this is the guarantee of one estimate over exact input |
| `post_asap` / `pre_asap` (modules) | post_asap: asap-physical-operators 139, asap-aware-mapping 87, integration-tests 72, types 64, devtools 51; pre_asap: asap-physical-operators 129, asap-aware-mapping 139, types 95, integration-tests 66 | `types::pre_asap` (schema, agg_intent, expr_ir, column_resolution), `types::post_asap` (sketch, guarantee, execution_data_state, maintained_population, summary_window); doc prose "pre-ASAP IR" / "post-ASAP DAG" in facade (`planner/src/lib.rs:2`), `pass/mod.rs:4`, crate lib.rs, and concept docs `concepts/pre-asap-ir.md`, `post-asap-ir.md` | M (#572: replace with `ir/` sections) |
| `ExecutionTiming` / `phase` | ExecutionTiming: types 102, asap-aware-mapping 20, integration-tests 18, asap-physical-operators 17 | `ExecutionTiming` itself is A (#573). "phase" meaning timing is M: `ir/timing.rs:710 split_shared_by_phase`, `node.rs:278` "Validate assigned phases", `timing.rs:46` "at either phase", `execution_data_state.rs:228`, tests `*_at_both_phases`. "Pane phase" / `evaluation_phase` (`summary_window.rs`, `workload.rs:270`) is a time offset — unrelated, keep it | M for timing-phase only |
| `MajorPass` / "two-phase" | asap-aware-mapping 10, planner 5 | `pass/major.rs:22 MajorPass`, `pass/mod.rs:1–9` "two-phase vocabulary", `PassRegistry::with_builtin` | M (#572 Q36 retired it) |
| `replacement` | asap-aware-mapping 955 (mostly module paths), devtools 150, integration-tests 91, types 88 | module `replacement` (11,466 lines), `explain_replacements`, `ReplacementExplanation`, `types/dag_export.rs:371 TargetReplacement`, `:433 TargetReplacementAfter` | "replacement" alone is close to the doc's "summary replacement"; the problem is that the module is the legacy Stage 1+3 search, not the word |
| "precompute" / "frontier" | asap-physical-operators | `physical_planner/precompute.rs`, `CompiledPhysicalPlan.precompute`, `enumerate_frontiers` | M `[judgment]`: the doc says MatAtIngestion / materialization assignment |

Pending PRs:
- **#561 (d4483945), new files:** `local` 0, `alternative` 2 (test name `cardinality_keeps_exact_specialized_and_universal_alternatives` and its doc line 81), `realiz` 7 (doc comments; doc-consistent "has a realization"), "falls back" 1 (`per_computation_candidates.rs:78`), `LogicalCandidateError` 15.
- **#575 (old names):** `local` 83, `alternative` 44, `realiz` 21 across `logical_candidates.rs`, its tests, `stage_pipeline.rs` and `stage1_composition.rs`. These are old #561 names and are not counted as findings. They go away when #575 is restacked on d4483945.

---

## 3. Architecture: where each stage lives today

### origin/main

| Stage | Where | Matches the doc's boundary? |
|---|---|---|
| 0 Frontends | `frontend-*` crates; facade `planner::lower` | Yes |
| 1 Pass 1 | Only the legacy search: `replacement.rs` (`ASAPStrategies`, `realizations_for_intent`, `search_workload_with_targets` :6435), plus strategy modules `grouping.rs` (Hydra), `rewrite.rs`, `rollup.rs`, `topk_reuse.rs`, `exact_composition.rs`, `maintained_population.rs` | **No.** Candidates are ranked (`realizations_for_intent` "ranked most-preferred-first via a CostModel", :874) and **sized** at generation (`sketch_realizations` → `cost_model.size_params`, :1019). Strategies take a `CostModel` (`default_strategies_with_evidence(models.cost, …)`). Construction keeps the cost-preferred child (`realize_child` "rank-and-take-first", :1887). `sketch_realizations` drops sketches over `DEFAULT_MAX_SKETCH_STATE_BYTES` (a resource rule, not a provable-accuracy prune) `[judgment]`. `fixed_window_rate_candidates` (:1418) applies a `MatAtIngestion` assignment inside Stage 1 to keep only candidates that compile at ingestion (Stage 2 reasoning in Stage 1). Calls to `cost_model.*` before the test module of `replacement.rs`: `allow_uncosted_legacy_selection` ×5, `candidate_cost` ×3, `estimate_cost` ×3, `cse_share_decision` ×2, `size_params`, `realize_extension`, … (178 lines mention cost_model/CostModel) |
| 1 Pass 2 | `types::ir::cse::share_common_sub_dags` (run *before* the search in `cse_workload`, and again after selection in `MajorPass`), `SharedSubDAGStrategy`, `accuracy/reconciliation.rs` (cost-aware: 8 CostModel refs), `pane_sharing.rs`, `recurrence.rs` (cost-aware CSE decision) | **No** separate pass. CSE share/recompute is decided by `CostModel::cse_share_decision` (selection inside Stage 1) |
| 2 Materialization | `types/src/ir/timing.rs` (`MaterializationAssignment`, `apply_materialization_timings`, `validate_at_ingestion`, `split_shared_by_phase`); `post_asap/execution_data_state.rs` (`Materialization`); applied only as the default assignment (all `NotMaterialized`) in `PlanOutput::execution_timed_dag` (`pass/mod.rs:232`) | The types and rules match the doc (#667). There is **no enumeration** of `Opt(s)` and no `CandidatePhysicalASAPDAGs`. Materialization is fixed at export, after selection — the reverse of the doc's order |
| 2 Physical operator impl | Executor: `asap-physical-operators/src/physical_planner/*` | Not in the planner. The executor also **enumerates materialization frontiers** (`physical_planner/candidates.rs:141 enumerate_frontiers`, "materialization frontier search") and **assigns** `MatAtIngestion` itself (`promql_rows.rs:93,210` "This compiler emits maintained precompute: every sub-DAG MatAtIngestion"). That conflicts with doc assumption 8 / §4 ("does not … decide what to materialize") |
| 3 Plan selection | `replacement.rs` `CandidateLogicalASAPDAGs::{cost_sorted, global_selection, global_selection_with_recurrence}`, `GlobalSelection::assemble_selected_query`; cost: `cost_model.rs`, `analytical_cost.rs`, `physical_plan_cost_model.rs`, `query_physical_lowering.rs`, `storage_io.rs`, `physical_handoff_cost.rs`; empirical accuracy: `erp.rs`, `empirical_*` | Selection works on logical per-target groups, not on whole physical candidates. It runs before Stage 2 (materialization comes after) |
| 4 Execution | `asap-physical-operators` | The planner does **not** execute: `planner` and `asap-aware-mapping` have no dependency on `asap-physical-operators` (checked in Cargo.toml). `empirical_*` reads offline evidence; nothing runs |
| Facade | `planner::e2e_plan` → `optimize(MajorPass)` | Does not run Stage 1 → 2 → 3 (#572 Q36 / D1 not on main) |

Answers to the specific questions (main):
- *Does Stage 1 code use cost models?* Yes: the legacy Stage 1 ranks and sizes with `CostModel`, and CSE share/recompute uses it.
- *Does Pass 1 rank?* Legacy: yes. #561: no (stable catalog order, "unranked"). One note: #561 orders summaries with `replacement::summary_candidates`, whose doc still says "most-preferred first" — a catalog order inherited from legacy ranking `[judgment]`.
- *Does anything outside Stage 3 discard valid candidates?* Legacy: yes. `realize_child` takes the first child candidate; `cse_share_decision` commits share vs recompute; `sketch_realizations` drops sketches over the byte cap; `global_selection` is called inside `MajorPass` before materialization. Executor-side frontier enumeration has a budget but errors out rather than truncating (good). #575 `stage_pipeline` caps output at `--max-candidates` (default 64) but records `"capped"` — a display cap in a devtool, acceptable `[judgment]`. #576 `stage2_physical` returns `Stage2Error::IngestionTimeOnly` for any candidate with a `MaintainPopulation`. After #667 that is wrong: populations can be `NotMaterialized`. It should be a pruned candidate with a reason, not an error for the whole set.
- *Does the planner execute anything?* No.

### Pending stack

- **#561** (Pass 1) is clean against the doc: no cost or accuracy model, unsized summaries, one exact option, `PruneReason`, and it rejects inputs with ASAP ops, timing or materialization (`AssignedPhysicalAnnotation`, :299–:302). Gaps:
  - It still imports helpers from the legacy module: `crate::replacement::{accuracy_budget, accuracy_target, summary_candidates}` (:37).
  - `ComputationTarget` records the node, filter and HAVING, but not the doc's explicit ANNOTATE fields (input expression, grouping, window, supported estimates, accuracy requirement). These can be derived from the node, which is fine until Pass 2 needs them `[judgment]`.
- **#575** (Stage 1 composition): the doc's lines 18–21 SUBSTITUTE step.
  - `composed_guarantee` calls `DefaultAccuracyModel.local_guarantee` — Stage 1 uses the built-in analytical model only. That matches #572 Q22 (analytical in Stage 1, pluggable model in Stage 3).
  - It **sizes summaries at composition** (`sized()` → `default_size_params`). Doc lines 472–476 say sizing happens after Pass 2 (strictest consumer). Composition runs before any Pass 2, so this is a small doc/code mismatch. Either document "sized for its own requirement here; Pass 2 re-sizes shared summaries" or move sizing `[judgment]`.
  - It also leans on the legacy module (`replacement::summarised_input`, `column_ref` made `pub(crate)`).
- **#576**: `physical_candidates::stage2_physical` (MVP: one physical candidate per logical one, all query time) and `plan_selection::stage3_select` (accuracy reject, then analytical price, then cheapest). The boundaries are right, but it predates #667 and does not enumerate `Materialization`.

### Relation to the approved #572 layout (memory `module-layout-572.md`)

Approved target: `types` (`ir/` by #511 sections, `workload/`, `physical/`) · `frontends` (Stage 0) · `logical-optimizer` (Stage 1: `pass1/`, `pass2/`, analytical `accuracy/`) · `physical-optimizer` (Stage 2) · `plan-selection` (Stage 3: `cost/`, empirical `accuracy/`) · `executor` (Stage 4) · `planner` facade.

Status on main: none of the stage crates exist yet. `asap-aware-mapping` still mixes Stage 1, Pass 2, Stage 3 cost, empirical accuracy and the legacy search. `types` still has `pre_asap/` and `post_asap/`. `MajorPass` still exists, even though Q36 retired it. The old pre-restack stack (#616–#641: `logical-optimizer`, `plan-selection`, deletion of legacy search/CostModel, AccuracyModel to plan-selection) carries those moves, but none of it is merged.

Where the pending code should land under #572:

| Module | Destination |
|---|---|
| `per_computation_candidates.rs` | `logical-optimizer/pass1/` |
| #575 composition | `logical-optimizer` (Stage 1 output assembly) |
| `ir/timing.rs`, Materialization types | `types::ir` (properties). Assignment *enumeration* goes to `physical-optimizer` |
| `physical_candidates.rs` (#576) | `physical-optimizer` |
| `plan_selection.rs` (#576), `cost_model.rs`, `analytical_cost.rs`, `physical_plan_cost_model.rs`, `query_physical_lowering.rs`, `storage_io.rs`, `erp.rs`, `empirical_*` | `plan-selection` |
| `accuracy/{estimators,composition,allocation}` | `logical-optimizer/accuracy` |
| `accuracy/reconciliation.rs` | `logical-optimizer/pass2` |
| `cse.rs`, `pane_sharing.rs`, `rollup.rs`, `topk_reuse.rs`, `grouping.rs` (sharing part) | `logical-optimizer/pass2` (`[judgment]` for `cse.rs`: it is an IR utility, but the doc places the identical-expression rule in Pass 2) |
| `physical_planner/` incl. `enumerate_frontiers` | `executor`. The frontier *enumeration* is Stage 2 work and should move to `physical-optimizer` or be deleted once Stage 2 enumerates `Materialization` `[judgment]` |

---

## 4. Prioritized fix list

### (i) Pending PRs

**#561 now (on d4483945):**
1. Push d4483945 (it is local only) and retitle the PR. Current title: "feat(planner): enumerate local logical alternatives without execution timing". Suggested: "feat(planner): Stage 1 Pass 1 per-computation candidate generation". Update the PR body to the new names.
2. `LogicalCandidateError` → `PerComputationCandidateError` (`per_computation_candidates.rs:155`, 15 uses in #561 files). Its variants are fine.
3. `per_computation_candidates.rs:78` doc "a pruned exact accumulator form falls back to plain `Exact`" → "is replaced by plain `Exact`" (no non-execution "fallback").
4. `tests/per_computation_candidates.rs:81,83` `cardinality_keeps_exact_specialized_and_universal_alternatives` → `…_candidates`.
5. Optional `[judgment]`: `ComputationCandidate::Summary(SketchAlgorithm)` doc "An unsized summary family" is fine as is.
6. Optional `[judgment]`: add a one-line note that `generate_per_computation_candidates` returns `Replacements(s)` per target (doc Algorithm 1 lines 3–17), and that the Cartesian product (lines 18–21) is Stage 1 composition (#575).

**#575 in its restack (onto the new #561):**
1. Adopt the #561 names everywhere. Mapping: `logical_candidates` → `per_computation_candidates`; `LocalCandidate` → `ComputationCandidate`; `LocalLogicalTarget` → `ComputationTarget`; `LocalChoices.alternatives` → `ComputationChoices.candidates`; `LocalLogicalCandidates` → `PerComputationCandidates`; `enumerate_local_logical_candidates` → `generate_per_computation_candidates`; `local_candidates_for_intent` → `computation_candidates_for_intent`. Apply in `stage_pipeline.rs:9–10,21–22,91,96,113,142,167,178–184` and `tests/logical_candidates.rs`, `integration-tests/tests/stage1_composition.rs`, `devtools/tests/stage_pipeline.rs`.
2. Delete the re-added `docs/develop_docs/local-logical-candidates.md` (#561 removed it). Put the composition rules in the module doc of the composition code instead.
3. Put composition in its own Stage 1 module rather than appending to the Pass 1 module `[judgment]`: e.g. `stage1_composition.rs` with `compose_logical_candidate` → `compose_workload_candidate`. Rename internal `realize`/`realize_measure`/`realized` → `substitute`/`substitute_measure`/`is_substituted` (doc Algorithm 1 SUBSTITUTE).
4. Sizing: either document in `compose_*` that a summary is sized for its own requirement here and Pass 2 re-sizes shared summaries (doc lines 472–476), or leave it unsized until Pass 2 `[judgment]`.
5. `stage_pipeline` header: "one per choice of a local alternative for every target" → "one per combination of per-computation candidates (the Cartesian product)".
6. `major.rs` comment edited by #575 ("Stage 3, #576"): fine, but it lives in `MajorPass`, which #572 retires.

**#576 in its restack (onto #575 + #667):**
1. Use `Materialization` / `MaterializationAssignment` from #667. Enumerate `Opt(s)` = {NotMaterialized, MatAtIngestion(m), MatAtQuery(m)} per Algorithm 3, or document the MVP limit.
2. Drop `Stage2Error::IngestionTimeOnly` ("maintained populations run at ingestion time"). Since #667 a population can be NotMaterialized. A candidate that cannot be planned should be pruned with a reason, not fail the whole set.
3. Names `[judgment]`: `stage2_physical` → `generate_physical_candidates`; `PhysicalCandidate` OK, with `Vec<PhysicalCandidate>` documented as `CandidatePhysicalASAPDAGs`; `stage3_select` → `select_plan`; `Stage2Error` → `PhysicalCandidateError`. Also rename the old `local_realizations_for_intent` and `LocalLogical*` names.

### (ii) Follow-up PR for merged code (renames/doc fixes, no moves)

1. `types/src/ir/asap.rs:36` `SummaryEstimate` doc: "Read out a query result from built summary state" → "Evaluate a query (`SketchStatistic`) on built summary state". `:42` `FinalizeExactAccumulator`: "the maintenance-to-read boundary" → "the exact accumulator state's finalized value".
2. "read out" in comments → "evaluate"/"evaluation": `post_asap/sketch.rs:536` ("Sketch read-out queries" → "Sketch evaluations"), `pre_asap/schema.rs:135`, `ir/scalar.rs:340`, `dag_export.rs:1125`, `post_asap/query_time/mod.rs:18`, `asap-aware-mapping/src/cost_model.rs:520`, `asap-aware-mapping/src/lib.rs:18`, `asap-physical-operators/src/physical_planner/mod.rs:411`. Also fix #573 lines 85, 252, 866, 952.
3. `types/src/post_asap/execution_data_state.rs`: `EvaluationUnderMaintenance` → `EvaluationUnderIngestion` (it fires when an evaluation feeds an ingestion-time state) and `InvalidMaintenanceBinary` → `InvalidIngestionBinary` `[judgment]`. Error strings to match. Doc :124–:147 "maintained summary" → "ingestion-time (MatAtIngestion) summary".
4. `types/src/ir/timing.rs`: `split_shared_by_phase` → `split_shared_by_timing`; doc rows :22 ("maintenance cannot align them" → "ingestion time cannot align them"), :45 ("when the state is maintained" → "when the state is MatAtIngestion"), :46 ("at either phase" → "at either timing"), :799, :912, :1099–:1100. `node.rs:278` "Validate assigned phases" → "Validate assigned timings". `physical_export.rs:456` `maintenance_dependency` → `ingestion_dependency`.
5. `asap-aware-mapping/src/pass/mod.rs:232` `PlanOutput::execution_timed_dag` → `physical_asap_dag` `[judgment]`. Module doc :1–:9 should drop "pre-ASAP IR in, post-ASAP DAG out" and "two-phase vocabulary" in favor of "Stage 0 LogicalDAG in, selected PhysicalASAPDAG out".
6. `planner/src/lib.rs`: module doc "selected post-ASAP DAG", "optimization pass", "pre-ASAP IR" → stage vocabulary; `let fallback = MajorPass` → `let default_pass = MajorPass` (until the facade runs Stage 1 → 2 → 3).
7. `asap-aware-mapping/src/lib.rs` crate doc: rewrite the "`asap-plan` — the cost-aware optimizer layer…" header and the Terminology table (Realization / Replacement / Search / Runtime placement) to map onto Stage 1 Pass 1/Pass 2 / Stage 3. Drop "local alternatives" (:34) and "Realization: Enumerate ranked physical forms".
8. `AccuracyModel::local_guarantee` (`accuracy/mod.rs:51`, impls :97, `estimators/mod.rs:58,196`, tests `local_guarantee_inverts_*`) → `estimate_guarantee` `[judgment]`: the guarantee of one estimate over exact input.
9. Cost-model "maintenance" names, if they survive #572's legacy deletion. Otherwise skip, because #635 deletes `CostModel`: `maintenance_operation_plan_cost_rate` → `ingestion_operation_plan_cost_rate`, `summary_maintenance_cost_per_update` → `summary_update_cost`, `default_cse_shared_maintenance_cost` → `default_cse_shared_update_cost`, `recurrence.rs:414 maintained_cost_rate` → `materialized_cost_rate`.
10. `asap-physical-operators/src/physical_planner/candidates.rs:1,15,203` and `mod.rs:55`: "maintenance-selected frontiers" / "Planner maintenance search/planning" → "materialization assignment selected by the planner (Stage 2)". `promql_rows.rs:93,210`: "maintained precompute" → "MatAtIngestion".
11. "alternative(s)" → "candidate(s)" in `replacement.rs` docs and tests (`repeated_roots_do_not_duplicate_alternatives`, `profiles_for_alternatives`, `shared_producer_handoffs_are_scoped_to_each_alternative`, `added_alternatives`). Only do this if the legacy search is not about to be deleted.
12. Concept docs: `docs/design_docs/concepts/planner-pipeline.md` (calls `CandidateLogicalASAPDAGs` "candidate Post-ASAP DAGs"; "ranking … not mandatory stages"), `glossary.md`, `pre-asap-ir.md`, `post-asap-ir.md`, `architecture/asap-aware-plan-search.md`. Point them at planner-layering.md stages, or mark them superseded.
13. #573 §6.1: add the `materialization: Materialization` field of `OperatorNode` and say `timing` is derived from it.

### (iii) Structural moves for #572

1. Retire `MajorPass` / `OptimizationPass` / `PassRegistry`. The facade runs Stage 0 → 1 (Pass 1, composition, Pass 2) → 2 → 3 (Q36/D1). Today `planner/src/lib.rs:169–176` still runs `MajorPass`.
2. Split `asap-aware-mapping` by stage:
   - `logical-optimizer`: `per_computation_candidates` → `pass1/`; composition; `accuracy/{estimators,composition,allocation}`; `pass2/` = `cse.rs` rules, `reconciliation.rs`, `pane_sharing.rs`, `rollup.rs`, `topk_reuse.rs`, Hydra grouping.
   - `plan-selection`: `cost_model.rs`, `analytical_cost.rs`, `physical_plan_cost_model.rs`, `query_physical_lowering.rs`, `storage_io.rs`, `physical_handoff_cost.rs`, `physical_operator_statistics.rs`, `erp.rs`, `empirical_*`, `AccuracyModel`, `PlanningModels` (Q22/Q37).
   - Delete `replacement.rs` (11,466 lines) once its helpers are moved. Pass 1/#575 currently need `accuracy_budget`, `accuracy_target`, `summary_candidates`, `default_size_params`, `summarised_input`, `column_ref` (old stack #631/#635).
3. The `CandidateLogicalASAPDAGs` name: free it from the legacy per-target inventory (`replacement.rs:4134`). Use it only for Stage 1's set of whole-workload `LogicalASAPDAG`s. Stage 3 methods (`cost_sorted`, `global_selection`) must not hang off a Stage 1 type (Q42).
4. Create `physical-optimizer` (Stage 2). It owns materialization-assignment *enumeration* (Algorithm 3) and physical operator implementation (e.g. TopK → sort+limit from #576). `MaterializationAssignment` / `apply_materialization_timings` stay in `types::ir` as properties and rules. Move or delete the executor's `enumerate_frontiers` (`physical_planner/candidates.rs:141`). Make `compile_current_series_evaluation` take the planner's assignment instead of forcing `MatAtIngestion` (doc assumption 8).
5. Rename crate `asap-physical-operators` → `executor`; no stage crate depends on it (already true for `planner` and `asap-aware-mapping`).
6. Split `types::pre_asap` / `types::post_asap` into `types::ir` sections per #511/#573. For example, `post_asap::execution_data_state` (`ExecutionTiming`, `Materialization`, `Medium`) → `ir/properties` (memory: "timing types to ir/properties"); `pre_asap::schema` → `ir/schema`; `post_asap::sketch` → `ir/summary`. Old paths have about 500+ use-sites across crates (see §2 counts).
7. Introduce named Stage 0/1/2 output types: `LogicalDAG` (= workload `Vec<(Id, QueryRoot)>` or `ParsedWorkload`), `LogicalASAPDAG`, `CandidateLogicalDAGs`, `CandidateLogicalASAPDAGs`, `CandidatePhysicalASAPDAGs`, so the stage signatures read like the doc's table `[judgment]`.
