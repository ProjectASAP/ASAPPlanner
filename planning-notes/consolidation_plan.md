# Upper-stack consolidation plan (ProjectASAP/ASAPPlanner, above #577)

This is a read-only planning document. Nothing has been pushed, posted, closed or edited.

Snapshot: 2026-10-13, after `git fetch` in `/mydata/ASAPPlanner-restack646-v3`, which is detached and clean. Inputs:

- `upper_stack_roadmap.md`
- the memory notes listed in the task
- `gh pr list` (59 open PRs by zzylol) and every upper PR's body
- `git diff` on `origin/*`

## 0. Facts that shape the plan

- **Open PRs.** 59 are open: #412, #574, #576, #577, #590, and the chain #578…#641. #552–#569 and #642 no longer appear in the open list, so they are already closed. main has #542, #665, #666, #543, #667, #561 and #575.
- **#576 is not the Stage 2/3 that the upper stack was built on.** Its head `60197da2` has 16 new commits, and it uses different names and semantics:
  - It adds the modules `physical_asap_optimization.rs` (`optimize_physical_asap`) and `plan_selection.rs` (`select_plan`). These replace the old `physical_candidates`.
  - The Stage 3 check reads `AccuracyEvidenceProvider::output_guarantee`.
  - A shared `NotMaterialized` node is charged once per consuming query.
  - Selection prices each composed candidate. There is no tree DP.

  Every upper PR that touches Stage 2/3 (#581, #588, #594, #604, #609, #625, #632) is built on the old MVP. Treat these as **re-implementations on #576**, not rebases.
- **Renames in #561/#575 that the upper PRs predate.**
  - `logical_candidates.rs` is now `per_computation_candidates.rs`.
  - `LocalCandidate` is now `ComputationCandidate`, `LocalChoices` is now `ComputationChoices`, and `LocalLogicalTarget` is now `ComputationTarget`.

  The roadmap's "LocalCandidate" is already stale.
- **Stage numbers in names.** main and #576 contain no `stage[0-3]_*` identifiers. The stack tip (`cleanup-12`) has about 130: `stage1_candidates`, `stage1_logical_candidates`, `stage3_select*`, `stage2_physical*`, the test name `stage2_b_*`, and others. Each group has to rename the ones it brings in (review-checklist rule).
- **`plan_stages` arrives in #588 (S2).** The S7 ports (#629, #633) and deletions (#630, #635) call it. So "S7 right after S1" already needs #588's single-library-path part.
- **#582 mostly moves the legacy selection.** About 2.9k lines go into `plan_selection/candidate_selection.rs`, and `replacement.rs` churns about 7.4k lines. S7 deletes that same code.
- **#641 conflicts with #576.** #641 deletes `PlanningModels.evidence`, but #576's Stage 3 now reads `evidence.output_guarantee`, which follows the 2026-10-13 decision.
- **Two decisions disagree.** Q44 says Stage 2 duplicates a shared node per consumer. The 2026-10-13 Example 4 decision says plan selection charges a shared `NotMaterialized` node once per consuming query, and Stage 2 does not duplicate it. #576 implements the second one.

## 1. Proposed merge order and groups (11 consolidated PRs)

### Order, and how it departs from the approved S1 → S7 → S2…S6

I propose: **S1 behavior (facade) → S7 (ports, deletions) → S1 moves (crate layout) → S2…S6.** In other words, I split S1 and put the S7 deletions *between* its behavior part and its move part.

Why this order is better:

1. **Less to move.** The S1 moves (#578, #583, #585) would carry `replacement.rs` (about 11.5k lines), `candidate_selection`, the legacy cost files, `dag_export` and the `promql_fallback` copies into new crates, and S7 then deletes them. Deleting first removes about 30k or more lines (estimate) from the move, so the move is smaller and easier to check with tools.
2. **#582 goes away.** Its 5.3k/−6.1k diff is mostly moving the legacy selection. With the deletions first, it closes. Only its cost-independence test survives.
3. **No extra rework.** #578 and #583 are being redone with a script anyway (an existing decision), so running the script after the deletions costs nothing extra.

The cost: the S7 ports and deletions are written against post-split paths, so they have to be redone on pre-split paths. Deletions are trivial to redo; the test ports (#629, #633) are moderate work.

If the user prefers the approved strict order (G3 before G1/G2), the groups stay the same and only G1–G3 change places. #588's `plan_stages` part still has to sit in G1. **This is open question Q1.**

| # | Group (conventional-commit title) | Absorbs | Review | Size (est.) |
|---|---|---|---|---|
| G1 | `feat(planner): run logical ASAP-aware optimization → physical ASAP-aware optimization → plan selection as the planner pipeline; retire MajorPass` | #581, #579, #588 (only the `plan_stages` library path), #582 (only the cost-independence test and the Pass 1 cost-import removal), #629, #633 | FULL | about +3k/−2.3k, about 60 files |
| G2 | `chore(planner): delete the legacy replacement search, legacy cost models, dag_export and the PromQL fallback copies` | #631 (pure move), #616, #617, #622, #624, #630, #635, #641 (shrunk), #637, the `promql_fallback` deletions | FAST-TRACK (docs: alignment check only) | about +1.2k/−37k |
| G3 | `refactor: arrange crates by planning stage (#572)` | #578, #583, #584, #585, #586, #587, #636 | FAST-TRACK | about 300–400 files, almost all renames; content about ±1.5k (import paths) |
| G4 | `feat(logical-asap-optimization): Example 1 — identical-expression CSE, whole-expression top-k heaps over counters, per-second plan-selection cost` | #588 (the rule), #589, #594, #593, #640 | FULL | about +2.8k/−0.8k |
| G5 | `feat(logical-asap-optimization): Example 2 — summary-capability CSE and UnivMon frequency statistics` | #595, #596, #597, #598, #599, #621, #627 | FULL | about +3.3k/−0.2k |
| G6 | `feat(logical-asap-optimization): Example 3 — window composition with tumbling panes` | #592, #591, #601 | FULL | about +3.6k/−0.3k |
| G7 | `feat(planner): grouped and filtered counts — row-counting COUNT(*), HydraCms, filtered aggregates` | #600, #603, #605, #607, #608 (tests), #620 (tests or closed) | FULL | about +2.6k/−0.2k |
| G8 | `feat(physical-asap-optimization): Example 4 — materialization of window summaries at ingestion or query time` | #604, #606 | FULL | about +2.3k/−0.3k |
| G9 | `feat(plan-selection): deployment inputs and kept query-time summaries (MatAtQuery)` | #609, #612, #625 (shrunk) | FULL | about +1.5k/−0.2k |
| G10 | `feat(logical-asap-optimization): shared window segments and segment-grid choice; TimeShift/TimeRange pricing` | #628, #632, #634 | FULL | about +0.9k/−0.3k |
| G11 | `feat(dag-viewer): stage-pipeline viewer, SQL editor and deployment inputs` | #574, #618, #638, #610, #613 | FULL (light: UI, devtool) | about +2.3k/−4.3k, plus one regenerated fixture |

Every group drops the per-PR `tools/dag-viewer/examples/*.json` diffs and regenerates the fixture once, in the group whose tests compare it. All consolidated PRs open as drafts (pr-priority-phases). Each original PR is closed with a comment that names its new PR (stack-speedup decision 1).

### G1: planner pipeline facade, with the legacy callers ported

- **Scope.** `StagePipeline` replaces `MajorPass` as the facade default (from #581). The pipeline is built on #576's `optimize_physical_asap` and `select_plan`, not the old MVP. It adds a single library entry, `plan_stages`, taken from #588 without the Pass 2 rule. #579's top-k selected-rows readout schema goes in, with Example 1 expectations re-derived against #577's spec. Pass 1 stops importing `CostModel` and `recurrence` for ordering and sizing (Q36). From #582 only the cost-independence test is kept, renamed. Every test and devtool that used `global_selection`, `search_workload` or `assemble_selected_dag` moves to `plan_stages` (from #629 and #633), or is deleted per its file decision.
- **Phase-2 fixes:**
  - Rename `fn fallback` to `select_by_enumeration`, and `let fallback` to `default_pass`.
  - Keep #561's `time_query_root` and scalar-root handling in `execution_timed_dag`.
  - No `assemble_selected_dag` call for query roots.
  - Use the #561/#575 names (`ComputationCandidate`, `PruneReason`, …).
  - Do not bring back "None ⇒ not auto-selectable".
  - Stage-name identifiers: `stage1_candidates` and `stage1_plan` in the #633 helpers, and the names in `stage_pipeline_selection`.
  - Tree DP versus #576's per-candidate pricing (Q3).
- **Review: FULL** (behavior change: selection, facade, test assertions).

### G2: legacy removal (fast-track)

- **Scope.**
  - Move Stage 1's realization helpers (`summary_candidates`, `default_size_params`, `accuracy_budget`, `column_ref`, …) out of `replacement.rs` into their own Pass 1 module (from #631, pure move).
  - Delete the `dag_export` bin and module, the legacy physical-plan cost model and `types::cost` leftovers, `show_post_asap_ir` (rename `show_pre_asap_ir` to `show_logical_dag`), the legacy `candidate_selection`, the legacy search, and the `CostModel`, `DefaultCostModel` and `empirical_cost` it ranked with.
  - Delete the remaining accuracy-evidence methods that nothing reads (#641, shrunk).
  - Delete the PromQL fallback copies: `asap-physical-operators/src/physical_planner/promql_fallback.rs`, its tests, its call sites in `physical_planner/mod.rs`, and its use in #577's `planner_layering_example1.rs`.
  - Bring the docs in line (from #637).
- **Phase-2 fixes:**
  - Remove the `promql_fallback` copies.
  - #641 keeps `PlanningModels.evidence` and `output_guarantee` (Q5).
  - Docs: #637's text is checked against #667's materialization wording and the stage names.
  - Check that #576's `analytical_cost` and `physical_operator_statistics` survive. They are not legacy; #576 uses them.
- **Fast-track verification:**
  - For each deleted public item, `git grep` finds no reference in the result.
  - `cargo nextest list` before and after: the change in test count equals the tests in the deleted files, listed in the PR body.
  - The #631 move passes `git diff -M --color-moved=dimmed-zebra` with no non-moved lines except `use` and `pub` changes.
  - Docs get an alignment read only.

### G3: crate layout by planning stage (fast-track)

- **Scope.** Redo #572 B1–B5 with a script on G2's head:
  - `asap-types` modules by #511 section (#578)
  - the logical-optimizer, physical-optimizer and plan-selection crates (#583, #584, #585)
  - the facade into `asap-planner` and deletion of `asap-aware-mapping` (#586)
  - `asap-physical-operators` renamed to `asap-executor` (#587)

  Then move `AccuracyModel` into plan-selection (#636, Q53). The built-in analytical rules that Stage 1 composition uses stay in the logical-optimizer crate (Q6).
- **Phase-2 fixes:**
  - Use the crate and module names decided in Q2.
  - Re-check that #585's three "no callers" modules still have none.
  - #636 drops `propagate` and `PlanningModels.cost`/`with_cost`, verified with a no-callers check.
- **Fast-track verification:**
  - `git diff -M100% --summary`.
  - Per-file blob comparison after normalizing crate paths: the diff must be empty except for `use`, `Cargo.toml` and `lib.rs` lines.
  - Identical `nextest list`.
  - Cargo dependency graph shows one-way flow (`cargo tree`).

### G4: Example 1 completion

- **Scope.**
  - The Pass 2 identical-expression rule as Stage 1 variants, with selection treating the variant as an outer choice (#588).
  - Whole-expression Count-Min and CountSketch + heap over raw samples, with absorbed targets (#589). Its partition fix comes from #640.
  - `metric_types` and the counter non-negativity proof for Count-Min + heap (#593).
  - Per-second plan-selection cost, memory price and `Stage3Calibration`, with `stage3-cost-model.md` (#594, S1, S2, S5). The design doc's name needs Q2's naming rule.
- **Phase-2 fixes:**
  - Port to `ComputationCandidate` and `per_computation_candidates`.
  - `MaterializationAssignment::all_query_time` becomes `Default`.
  - Follow the renamed fallback call sites.
  - Per-second cost must compose with #576's per-consumer multiplier for `NotMaterialized` shared nodes (the Example 4 cost decision).
  - Fix-list item: the Pass 1 Top-K sketch update uses the sample value as its weight, so SQL with an Int64 key, or a key named `value`, breaks. Fix it here or open an issue.
  - Edits to the Example 1 acceptance doc go into #577's merged spec.
- **Review: FULL.**

### G5: Example 2 (summary-capability rule and UnivMon)

- **Scope.**
  - The Pass 2 summary-capability rule, all or nothing per key and sized for the strictest consumer (#595, W5).
  - The `FrequencyMoments` key, so one UnivMon serves distinct, L2 and entropy (#596).
  - Executor support for exact distinct, L2 and entropy, with typed UnivMon ids (#597).
  - SQL L2 and entropy idioms (#598).
  - Executor exact SQL entropy and Example 2 status (#599).
  - UnivMon L2 certified from layer 0, with real UnivMon pricing (#621, Q64).
  - Plan selection's `answers` check on the error metric (#627, Q65).
- **Phase-2 fixes:**
  - Move summary sizing to after Pass 2 and size for the strictest consumer (fix list, "Summary sizing").
  - #627 must follow #575/#576's rule that a None guarantee is decided by evidence.
  - Rename #599's "SQL entropy fallback" wording. It is native executor execution, not a planner fallback (*uncertain*, Q11).
  - Remove the executor call sites next to `promql_fallback`.
- **Review: FULL.**
- **Ordering caveat.** I moved #621 and #627 earlier, out of S5. In the chain they sit on #618 and #574, but their file lists (executor, accuracy, plan-selection) show no dependency on those PRs. *Moderately confident.*

### G6: Example 3, tumbling panes

- **Scope.**
  - Coverage time relative to evaluation, and `family_merges` with overlap rules (#592, W2, W6).
  - Executor tumbling pane merges (#591).
  - Pass 2 window composition: `WindowForm::Tumbling`, `pane_ms = gcd(lookback, cadence)`, and `SummaryMerge` timing (#601, W1, W3, W4).
- **Phase-2 fixes:**
  - #601 ports from `Realization` to `ComputationCandidate`.
  - Use the #667 timing API (`Materialization`, `set(node, …)`, `validate_at_ingestion`).
  - Put #590's Example 3 tests into #577's spec layout.
- **Review: FULL.**
- **Ordering caveat.** #592 and #591 sit before #595–#599 in the chain. Moving them after G5 assumes G5 does not use `family_merges` (*uncertain*; check while restacking).

### G7: grouped and filtered counts

- **Scope.**
  - The HydraCms executor kernel (#600), and Pass 1 offering HydraCms for grouped counts, with grouping-aware analytical checks (#605, W7).
  - SQL `COUNT(*)` counts rows (#603). It may already be partly covered by #575's `Constant(1.0)` weight for Count + CMS/CS/UnivMon; what probably remains is the exact accumulator path.
  - Executor filtered aggregates and summary builds (#607, Q47).
  - #608 reduced to tests: `filtered_aggregates.rs`, the `summary_coverage_e2e` example and `stage_pipeline_selection`. #575 already holds its realization rule.
  - #620 reduced to tests, or closed (Q8).
- **Phase-2 fixes:**
  - #605 TopK Hydra.
  - Fix list: SQL `count(*)` counts a projected literal 1.0 column under the `SummaryAgg`, and `AvgAsSumCount` becomes Join + Project divide.
  - #620 keeps candidates. Prune only the provably invalid ones, with a `PruneReason`.
  - Remove the executor call sites next to `promql_fallback`.
- **Review: FULL.**
- **Ordering caveat.** I moved this group earlier than #609 because #609's tests touch `filtered_aggregates.rs`.

### G8: Example 4, materialization

- **Scope.**
  - Stage 2 ingestion-time eligibility (S4).
  - `MatAtIngestion` versus `MatAtQuery`.
  - The latency check on the query-time cut (S6).
  - Cross-root phase split (#604).
  - Example 4 acceptance with materialization (#606).
- **Phase-2 fixes:**
  - Adopt #667's `Materialization`: `set` accepts any node.
  - `execution_timed_dag` exports the selected plan's assignment instead of `all_query_time()`.
  - #604's multi-root `split_shared_by_phase` conflicts directly with #667's rewrite.
  - Follow the Example 4 cost decision rather than Q44 (Q4).
- **Review: FULL.**

### G9: deployment inputs and kept query-time summaries

- **Scope.**
  - `DeploymentCapabilities` and raw-data retention pricing (#609: C1–C5, Q48).
  - Example 4's B1 versus B2 as model-relative invariants, plus the B1-wins workload (#612, Q49).
  - #625 reduced to Stage 2 emitting `MatAtQuery` for kept tumbling panes (B3), with storage and reuse pricing, gated by `query_time_retention` (Q56, Q57, Q59).
- **Phase-2 fixes:**
  - #625's `kept` flag, `set_kept`/`is_kept` and the `physical_export` field are dropped in favor of `MatAtQuery(Medium)` (decided).
  - Rename the test names `stage2_b_*`.
- **Review: FULL.**

### G10: window segments

- **Scope.**
  - Year-aligned shared segments for Example 3 Pattern A and 4a (#628, Q60).
  - TimeShift priced free, and TimeRange priced by the rows it keeps (#632, Q66).
  - The planner chooses the segment grid through Stage 1 variants (#634, Q67).
- **Phase-2 fixes:**
  - #632's comments cite `promql_fallback.rs`. Rewrite them after G2.
  - Port to the #667 timing API.
  - Use stage-name test names.
- **Review: FULL.**

### G11: viewer and devtools

- **Scope.**
  - The viewer redesign around `asap-stage-pipeline/v1` (#574).
  - The SQL editor over declared tables (#618, Q54).
  - Vertical lanes (#638).
  - The deployment section in the stage document and viewer (#610).
  - `stage_pipeline` examples `planner-layering-2` and `-4b` (#613).
- **Phase-2 fixes:**
  - #574 takes the delete for `render.py` and `dag.example.json` (modify/delete).
  - Regenerate the fixtures once.
  - Viewer labels use stage names (#576 already renamed the pipeline keys).
  - #610 has to target the redesigned viewer, not the old one.
- **Review: FULL (light).** #574 is mostly deletions and UI. Rebuild the claude.ai artifact afterwards (dag-viewer-redesign).

## 2. Close, or shrink drastically

### Close without porting

- **#590.** Already decided: its Example 2–4 specs are ported into #577, and its tests are already ported into #599, #601 and #606. Close it after #577 merges.
- **#582.** Superseded by the deletion-first order: about 95% of its diff moves the legacy selection that G2 deletes. Its cost-independence test (`stage1_cost_independence.rs`, renamed) and the Pass 1 cost-import removal are re-done in G1. *This depends on Q1; under the strict order it is absorbed into G3 instead.*
- **#579 (partial, uncertain).** Its "Example 1 expects 24 → 24 → 1" part is superseded by #577's spec, whose doc counts are 3 / 54 / 156 / 1. Its top-k readout schema change may conflict with #576's `60197da2` ("exact top-k roots output the logical `[keys.., topk_k]` columns") (Q7). The schema part is kept in G1 for now.
- **Already closed:** #552–#569 and #642. They are not in the open list. Nothing more to do.

### Shrink drastically (fold in only the remainder)

| PR | What remains | Group |
|---|---|---|
| #608 | Tests only. The `SummaryAgg.filter` realization rule is in #575. | G7 |
| #620 | Tests, or nothing. Its `retain(!Sketch)` prune is removed (keep candidates). | G7 |
| #625 | `MatAtQuery` emission plus pricing. The `kept` flag and its IR and export field are dropped. | G9 |
| #641 | Only the evidence methods nothing reads. `PlanningModels.evidence` and `output_guarantee` stay. | G2 |
| #603 | Probably only the exact or accumulator `COUNT(*)` path plus tests (*uncertain*). | G7 |
| #588 | Split: `plan_stages` goes to G1, the rule to G4. | G1, G4 |
| #587 | The `promql_fallback.*` renames are dropped; the files are deleted in G2. | G3 |
| #637 | Doc lines for features that do not exist yet (calibration, capabilities) move to the group that adds each feature. | G2 |

## 3. Open questions for the user (to decide before building the groups)

1. **Order.** Facade and ports (G1) → deletions (G2) → crate moves (G3), instead of the approved S1 (moves included) → S7? This also closes #582.
2. **Crate and module names under the stage-name rule.** Keep #572's `asap-logical-optimizer`, `asap-physical-optimizer` and `asap-plan-selection`? Or name them after the stages, e.g. `asap-logical-asap-optimization`? Are the `pass1/` and `pass2/` directories acceptable, or should they be `per_computation_candidates/` and `asap_aware_cse/`? What should `stage3-cost-model.md` be called?
3. **Selection algorithm.** Keep #581's tree DP with the coupling guard (Q38–Q41) on top of #576's `select_plan`, which prices each composed candidate with per-consumer multipliers? Or keep #576's pricing of every candidate for now and move the DP to a later PR? Does the DP's additive assumption still hold with the multipliers?
4. **Q44 versus the 2026-10-13 Example 4 cost decision.** Confirm the latter supersedes Q44: no Stage 2 duplication, and a shared `NotMaterialized` node is charged once per consuming query. #604 and #606 may assume Q44.
5. **#641.** Keep `PlanningModels.evidence` and `AccuracyEvidenceProvider::output_guarantee`, because #576's Stage 3 reads them, and delete only the unread methods?
6. **#636 and Stage 1.** #576's decision has Stage 1 composition use the built-in theoretical model. With `AccuracyModel` moving to plan-selection, confirm the split: built-in analytical rules stay in the Stage 1 crate, and the trait plus `DefaultAccuracyModel` go to plan-selection.
7. **#579 and #576.** Which top-k output schema is canonical: selected rows, or the logical `[keys.., topk_k]`? Do we drop #579's Example 1 expectation changes in favor of #577's spec?
8. **#620.** "Prune only the provably invalid" with a `PruneReason`: does an executor binding failure count? Or does it belong to Stage 3's deployment-capability rejection (#609), which would mean #620 closes and only its tests survive?
9. **#625.** Does `MatAtQuery(Medium)` fully express B3 "kept, N−1 panes, cold start unpriced" (Q58, Q59)? Does the `query_time_retention = false` gate still mean B3 is generated but rejected (Q56)?
10. **Review type for test ports and docs.** Are #629/#633 (inside G1) and #637 (inside G2) covered by G1's full review and G2's alignment check, or does each need its own review?
11. **Naming of executor "fallback".** #599's "exact SQL entropy fallback" is native executor execution. Is the rename enough, or should the path be treated as a planner fallback and dropped?
12. **Group size.** G5 and G6 are each about +3.3k–3.6k. Is one review each acceptable, or should G5 split into planner (#595, #596, #621, #627) and runtime (#597, #598, #599)? That split would give 12 groups.

## 4. Must stay untouched

- **#412** (an open draft, user-owned): don't comment on it, close it, restack it or fold it into anything.
- **#649** (not by zzylol): ignore.
- **#576 and #577**: they go through their own merge pipeline. Fold nothing into them except the decided port of #590's specs into #577.
- **Already closed** #552–#569 and #642: don't reopen them.
- **Milind-assigned issues** (#156, #158, #162, in other repos): no code.
- **Existing branches and worktrees**: don't rewrite them in place. Build the consolidated branches as new branches. Push `origin/backup/<branch>-before-consolidation` refs before closing any original.
- **Merge rule**: merging upper PRs needs the user's OK. The #542–#561 authorization does not cover them.
