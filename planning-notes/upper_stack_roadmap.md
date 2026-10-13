# Upper-stack restack roadmap (above #561)

Snapshot taken 2026-10-13 after `git fetch` in `/mydata/ASAPPlanner-restack646-v5`, which stayed detached at `5c2bd852` and clean. Sources: `gh pr list` (78 open PRs by zzylol), `git log`/`git diff` on `origin/*`, the memory notes and `pr_review_guide.md`.

## 0. State of the bottom

- main already contains #542, #665 and #666 (merged 2026-10-13). On GitHub, #543's base is still `stack/528-07c-planner-followups`, a branch that has been merged; retarget it when #543 moves.
- #543 → #667 → #561. On origin, `stack/528-05b-local-alternatives` is `d18125b8`. The new local #561 head `8817d847` has **not been pushed yet**: it is not an ancestor of origin 05b.
- #575 (`db2f3c4f`) still sits on the **old** #561. That old tip is about `7c8ae7d3` ("validate local candidates with validate_structure"), and it is not an ancestor of origin 05b. #575 → #576 → #577 are being restacked separately.
- Code that changed between the old #561 (`7c8ae7d3`) and the new one (`8817d847`): 121 files, +15,028/−4,645. These are the conflict hot spots:
  - **Timing API** (`types/src/ir/timing.rs`, +587/−118). `MaterializationAssignment` now holds `Materialization {NotMaterialized, MatAtIngestion(Medium), MatAtQuery(Medium)}` for *any* node. `set(node, Materialization)` replaces `set(summary, ExecutionTiming)`. `all_query_time()`, `all_ingestion_time()` and `summary_timing()` are gone; use `Default` or `with_default`. `validate_maintained` is now `validate_at_ingestion`. `split_shared_by_phase` was rewritten.
  - **Pass 1 API** (`logical_candidates.rs`, +313/−65). `Realization` is replaced by `LocalCandidate {Exact, ExactAccumulator, Summary, AvgAsSumCount}` plus `LocalChoices` and `PruneReason`. `local_realizations_for_intent` is now `local_candidates_for_intent`. HAVING is recorded as `HavingEntry`/`HavingPlacement`.
  - **Search and pass** (`replacement.rs` +295/−96, `pass/mod.rs`, `pass/major.rs`). `search_workload` has a new signature and returns `SearchError`. `is_logical_rewrite` is now a method. `execution_timed_dag` goes through `time_query_root` and handles scalar roots.
  - **Fallback removal.** `promql_fallback.rs` and its tests were deleted (−2,504 lines). Their copies above #561 remain in `executor/src/physical_planner/{mod.rs,promql_fallback.rs}` and in `planner_layering_example1.rs`, which comes from #577.
- **Still present in the new #561, so no cleanup PR is obsolete:** `dag_export` (bin and module), `show_post_asap_ir`, `physical_plan_cost_model.rs`, `cost_model.rs`, `global_selection`/legacy search, and `MajorPass`. `post_asap_dag`/`QueryExpr` were already gone in the old #561, so no upper PR touches them.

## 1. Tree

```
main ─┬─ #412  fix/per-entity-avg-rewrite          (parked draft, LEAVE)
      └─ #642  docs/planner-layering-mvp-status    (status doc, stale)
stack/528-09-accuracy (16 behind main) ─ #552→#553→#554→#555→#556→#557→#559→#562→#563→#564→#565→#566→#568→#569   (parked, superseded)
…#543→#667→#561→#575→#576→#577→ #578→#579→#581→#582→#583→#584→#585→#586→#587→#588→#589 ─┬─ #590 (side branch, diverged 76/76)
                                                                                          └─ #594→#593→#592→#591→#595→#596→#597→#598→#599→#601→#600→#604→#606→#603→#605→#607→#608→#609→#610→#612→#613→#574→#620→#618→#621→#627→#625→#628→#632→#634→#616→#617→#622→#624→#629→#630→#631→#633→#635→#636→#637→#638→#640→#641
```

- Above #577 there are **57 PRs on one linear chain (#578 … #641), plus #590**, the only side branch.
- Off-stack: #412, #642, and the #552–#569 chain (14 PRs).
- #649 is open but not by zzylol, so it is ignored here.

## 2. Per-PR table, in restack order

The table uses these abbreviations:

- **c**: own commits, counted as `origin/<base>..origin/<head>`.
- **diff**: the PR's own diff (`base...head`), with the `tools/dag-viewer/examples/*.json` fixture left out. Rows marked † also regenerate that fixture. Regenerate it after the restack instead of merging it.
- **Risk** (conflict risk against the rewritten bottom):
  - **VH**: very high.
  - **H**: high, either from overlap with the files rewritten old→new #561 (after mapping paths through the #578/#583–#587 moves) or from use of a removed API.
  - **M**: medium.
  - **L**: low.
- **Fix**: phase-2 fixes from `phase2-restack-fix-list.md` and related memory.

| # | c | diff (files, +/−) | Risk | What it does | Fix and hot spots |
|---|---|---|---|---|---|
| **Segment S1: MVP tail and #572 crate layout (critical path)** | | | | | |
| 578 B1 | 1 | 215, +1283/−1928 | **VH** | Moves `asap-types` modules into the #511 section layout (`timing.rs` → `ir/properties/timing.rs`, etc.) | 76 rewritten files. 69 files fail `git apply --check` on 8817d847. It carries old timing API names (`all_query_time`, `validate_maintained`, …). **Redo the move mechanically on the new base** instead of rebasing hunks. |
| 579 | 3 | 4, +392/−98 † | L | Top-k sketch readouts return the selected rows; Example 1 expectations follow the planner | Applies to 8817d847 except for paths from #578/#577. |
| 581 D1 | 4 | 17, +1484/−345 † | **H** | Stage pipeline facade, tree-DP selection; retires MajorPass | Touches `pass/mod.rs`, `major.rs`, `logical_candidates.rs`, `replacement.rs`, the `search_workload` signature and `execution_timed_dag` (it must keep #561's `time_query_root` and scalar roots). **Rename here:** plan-selection `fn fallback` → `select_by_enumeration`, and `let fallback = StagePipeline` (currently `= MajorPass` in #561) → `default_pass`. Retiring MajorPass also makes the #542 `same_reference` nit moot. |
| 582 D2 | 5 | 35, +5303/−6083 | **VH** | Stage 1 no longer depends on the cost model (Q36); moves the legacy selection out of `replacement.rs` | Heavy `search_workload`/`Realization` churn (77 lines). Touches `has_missing_accuracy_evidence`, `is_automatically_selectable` and `realize_binary`, which #575's fix removes or changes ("None ⇒ not auto-selectable" is dropped). Expect hunks that are no longer needed. |
| 583 B2 | 3 | 103, +2839/−2488 | **VH** | Extracts the `asap-logical-optimizer` crate | Moves `logical_candidates.rs`, `replacement.rs` and others that #561 rewrote. Redo mechanically. |
| 584 B3 | 1 | 14, +143/−15 | L | Extracts the `asap-physical-optimizer` crate | Pure move. |
| 585 B4 | 2 | 63, +346/−2215 | H | Extracts the `asap-plan-selection` crate; deletes three modules with no callers | Moves the cost files and `fn fallback`. Re-check that the deleted modules still have no callers. |
| 586 B4b | 1 | 30, +54/−115 | M | Moves the pass facade into `asap-planner`; deletes `asap-aware-mapping` | `planner/src/lib.rs` (`let fallback`), `show_post_asap_ir`. |
| 587 B5 | 1 | 99, +207/−226 | M | Renames to `asap-executor` | Renames `promql_fallback.*`, which #561 deleted. **Drop those files.** The executor copy and the Example 1 copy go per the "Everywhere" fix. |
| **Segment S2: W1, Pass 2 sharing and Stage 3 cost** | | | | | |
| 588 | 2 | 11, +791/−298 † | M | Pass 2 identical-expression rule | Uses the `enumerate_local_logical_candidates` API; touches the `fallback(...)` call sites (follow the #581 rename). |
| 589 | 2 | 7, +598/−165 † | **H** | Whole-expression top-k heap over raw samples | Edits `pass1/logical_candidates.rs` and `replacement.rs`, and uses `local_realizations_for_intent` and `Realization`. |
| 594 | 3 | 11, +925/−152 † | M | Stage 3 per-second cost, memory price, calibration and design doc | `MaterializationAssignment::all_query_time` in tests (API gone); `fallback(...)` call sites. |
| 593 | 3 | 14, +438/−144 † | M | Counter `metric_types` declaration; CMS non-negativity proof | `logical_candidates.rs` (+ its tests). |
| 592 | 3 | 4, +276/−14 | L | Coverage time relative to evaluation; `family_merges` | — |
| 591 | 4 | 2, +398/−1 | L | Executor tumbling pane merges | `executor/physical_planner/mod.rs`, near the fallback code that has to be deleted. |
| 595 | 2 | 11, +613/−72 | M | Pass 2 summary-capability rule (strictest consumer) | `logical_candidates.rs`, `reconciliation.rs`. |
| 596 | 1 | 3, +175/−6 | L | One UnivMon for distinct, L2 and entropy | — |
| 597 | 2 | 14, +776/−22 | M | Executor: exact distinct, L2 and entropy; typed UnivMon ids | `executor/physical_planner/mod.rs`, `query_physical_lowering.rs`. |
| 598 | 2 | 10, +801/−2 | L | Frontend recognizes the SQL L2 and entropy idioms | — |
| 599 | 3 | 9, +468/−11 | L–M | Executor exact SQL entropy; Example 2 status | `executor/physical_planner/mod.rs`. Check that "SQL entropy fallback" is not a planner execution fallback (it is executor runtime, so probably fine; *uncertain*). |
| **Segment S3: W2, windows, Hydra and materialization** | | | | | |
| 601 | 4 | 17, +2888/−307 † | **H** | Pass 2 window composition with tumbling panes | `ir/properties/timing.rs`, `logical_candidates.rs`, `Realization` (11 lines), the old timing API. |
| 600 | 3 | 12, +1023/−7 | L | HydraCms executor kernel | `executor/physical_planner/mod.rs`. |
| 604 | 2 | 17, +1824/−234 † | **VH** | Stage 2 materialization: ingestion time vs query time, latency check | **Fix (list):** adopt #667's `Materialization` (`set` accepts any node, not just `SummaryAgg` plus `summary_timing`), and fix the `execution_timed_dag` re-timing: the tip still uses `MaterializationAssignment::all_query_time()` and should export the selected plan's assignment. It rewrites `split_shared_by_phase` to take several roots, and #667 rewrote that same function, so expect a direct conflict. |
| 606 | 2 | 4, +525/−39 | L | Example 4 acceptance with materialization | Tests follow #604. |
| 603 | 2 | 6, +241/−9 | M | SQL `COUNT(*)` counts rows (unit-weight update) | `logical_candidates.rs`, executor `mod.rs`. Check whether #561/#575 now cover `COUNT(*)` (*uncertain*). |
| 605 | 2 | 9, +438/−37 | **H** | Pass 1 offers HydraCms for grouped counts | **Fix: TopK Hydra.** Large edits to `logical_candidates.rs` and grouping on the `Realization` API. |
| **Segment S4: W3, filters and deployment inputs** | | | | | |
| 607 | 2 | 11, +750/−119 | M | Executor filtered aggregates and summary builds | `non_asap.rs`, executor `mod.rs`. |
| 608 | 3 | 5, +350/−42 | M | Pass 1 single-measure filtered aggregates | **Shrinks:** the `SummaryAgg.filter` realization rule (in `logical_candidates.rs` and probably `asap.rs`) moves into #575, leaving mostly tests: `filtered_aggregates.rs`, the `summary_coverage_e2e.rs` worked example (#561/#666 also rewrote that file) and `stage_pipeline_selection`. |
| 609 | 3 | 18, +989/−45 | L | `DeploymentCapabilities`, raw-data retention (C1–C5, Q48) | Not in #561. |
| 610 | 1 | 4, +96/−4 † | L | Deployment inputs in the stage document and viewers | — |
| 612 | 2 | 2, +111/−19 | L | Example 4 B1 vs B2 as invariants (Q49) | — |
| 613 | 2 | 2, +307/−39 | L | `stage_pipeline` examples 2 and 4b | — |
| **Segment S5: viewer and post-demo fixes** | | | | | |
| 574 | 9 | 22, +1613/−4233 † | M | Viewer redesign around the stage pipeline | Deletes `render.py` and `dag.example.json`, which #561 modified (modify/delete conflict; take the delete). |
| 620 | 3 | 4, +240/−73 | M | Pass 1: no per-group sketch for SQL `COUNT(*)` | **Fix: keep valid candidates** (no cost-based drop in Stage 1). It currently does `retain(!Sketch)` on the old `Realization` list. Rework it as a `PruneReason` only for candidates that cannot be bound, or drop the prune and let Stage 3 price them. The exact intent needs the user (*uncertain*). |
| 618 | 1 | 7, +280/−41 | L | SQL editor over declared tables (Q54) | — |
| 621 | 2 | 11, +363/−52 | M | UnivMon certifies L2 from layer 0 (Q64) | Executor `mod.rs`, `summary_sharing.rs`. |
| 627 | 1 | 3, +84/−0 | L | Stage 3 `satisfies` checks the error metric | Interacts with #575's "Stage 3 decides None-guarantee candidates" rule; re-check `answers`. |
| **Segment S6: window follow-ups** | | | | | |
| 625 | 2 | 22, +612/−275 † | **H** | Stage 2 "query time, kept" (B3) | Adds a `kept` flag (`set_kept`/`is_kept`) to `MaterializationAssignment` and to `physical_export`. **This overlaps #667's `MatAtQuery(Medium)`**, which already means "computed at query time, stored across executions". It probably reduces to emitting `MatAtQuery` plus pricing (*needs a user decision*). |
| 628 | 2 | 12, +428/−81 | M | Shared year-aligned window segments (Q60) | `logical_candidates.rs`, `Realization`. |
| 632 | 3 | 5, +113/−34 † | L | TimeShift priced free; TimeRange priced by the rows it keeps (Q66) | — |
| 634 | 2 | 9, +393/−173 | M | The planner chooses the segment grid (Q67) | `logical_candidates.rs`. |
| **Segment S7: legacy cleanup** (none is obsolete; every target still exists in 8817d847) | | | | | |
| 616 | 1 | 6, +5/−3214 | L | Deletes the `dag_export` bin | Modify/delete conflicts: take the delete. |
| 617 | 1 | 11, +25/−1863 | L–M | Deletes the `dag_export` module | `viewer_contract.rs`, `exact_composition.rs`, `explanation.rs`, `replacement.rs`. |
| 622 | 1 | 5, +10/−1820 | L | Deletes the legacy physical-plan cost model | — |
| 624 | 1 | 5, +17/−262 | L | Deletes `show_post_asap_ir`; renames `show_pre_asap_ir` → `show_logical_dag` | — |
| 629 | 1 | 15, +248/−225 | M | Ports the tests to `plan_stages` | 12 rewritten files, including the deleted `executor/tests/promql_fallback.rs` (drop it). |
| 630 | 1 | 9, +18/−6441 | M | Deletes the legacy `candidate_selection` | `search_workload` and `assemble_selected_dag` users. |
| 631 | 2 | 22, +590/−580 | M–H | Moves Stage 1's realization helpers out of the legacy search (pure move) | It moves `Realization`-era helpers, so it must move #561's `LocalCandidate` code instead. |
| 633 | 3 | 15, +657/−1296 | M–H | Ports the legacy-search callers to Stage 1 | 12 rewritten test files. |
| 635 | 2 | 31, +103/−17994 | **H** | Deletes the legacy search and `CostModel` | 191 `Realization` lines, `has_missing_accuracy_evidence`, `realize_binary`, `assemble_selected_dag`. It is a deletion, so mostly "take ours = delete" after #575's changes. |
| 636 | 1 | 12, +199/−180 | L | Moves `AccuracyModel` to plan-selection | — |
| 637 | 2 | 35, +391/−912 | L | Docs: describe the stage pipeline | Re-check the text against #667's materialization wording. |
| 638 | 1 | 3, +13/−10 | L | Viewer vertical lanes (CSS) | — |
| 640 | 1 | 2, +120/−27 | M | Whole-expression top-k partitions go on the heap input | `logical_candidates.rs`. |
| 641 | 1 | 10, +30/−433 | L | Removes the unread accuracy-evidence API | — |

**Fallback removals and renames that apply across PRs** (fix list "Everywhere" plus `planner-no-execution-fallback`):

- Delete `executor/src/physical_planner/promql_fallback.rs` and its tests. #587 renames them, and its parent #577 brought them in. Remove their call sites in the executor's `physical_planner/mod.rs`, which are scattered through #591, #597, #599, #600, #603, #607 and #621.
- Delete the `promql_fallback` use in `planner_layering_example1.rs` (from #577). Upper PRs that edit this file: #578, #579, #583–#585, #587–#589, #593–#595, #601, #604, #609, #625 and #629.
- Rename `fn fallback` and `let fallback` in #581. The frontend-common resolver `fallback` → `usage_derived_schema` already exists in #561 itself, so it belongs below the upper stack.
- The stack tip makes no `assemble_selected_dag` calls for query roots: #635 deletes the remaining calls. While restacking, check that no PR before #635 adds a new one for roots.

## 3. Obsolete or superseded: close candidates (flag only; nothing has been closed)

| PR | Reason | Confidence |
|---|---|---|
| #552, #553, #554, #555, #556, #557, #559, #562, #563, #564, #565, #566, #568, #569 | Parked chain on `stack/528-09-accuracy` (16 behind main). All are ported or reimplemented upstack per W8 and the review guide: #597 (552/557/559/563), #598 (562/564), #599 (565), #591/#601/#604 (566/569), #581/#588 (568), #609 (553), #604 (554/556), #560/#592 (555). Memory listed them as parked "until Phase C", which is now done. | High; still needs the user's OK. |
| #590 | Side branch off #589, 76 commits ahead and 76 behind. Its tests were ported into #599, #601 and #606. **However, its `planner-layering-example{2,3,4}-acceptance.md` specs exist only in #590** (the chain tip has only the Example 1 spec). Port the specs into a small docs PR, or fold them in, then close it. | Medium. |
| #642 | Status report on main, written 2026-10-05 against the 67-PR stack. It is stale after the #646 and phase-1 rewrites. Close it, or rewrite it after phase 2. | Medium (user decision). |
| #625 (partial) | The `kept` flag duplicates #667's `MatAtQuery`. The Stage 2 choice and pricing stay. | Medium, uncertain. |
| #608 (partial) | The realization rule moves to #575; the tests stay. | High (decided). |

**Must stay untouched:** #412 is an open draft: don't comment on it, close it or restack it. No PR here implements a Milind-assigned issue (those are sketch-bench/ASAPQuery #156/#158/#162, in other repos). The #552–#569 chain is parked: don't restack it.

## 4. Batches: what can run in parallel and what must be sequential

The restack itself is **strictly sequential**: #578 … #641 is a single chain, and each PR's new base is its parent's new head. The only independent subtrees are trivial:

- **Parallel now, before #577 finishes:**
  - (a) Decide on and draft the #590 spec port.
  - (b) Decide what to do with #642.
  - (c) Prepare the close list for #552–#569.
  - (d) Draft the design-level fixes as standalone notes or patches against today's heads, so each can be applied when the restack reaches its PR:
    - #604 `Materialization` adoption and `execution_timed_dag`;
    - #605 TopK Hydra;
    - #620 keep-valid rework;
    - #625 `kept` → `MatAtQuery`;
    - the #581 renames.
- **Sequential frontier, in segments with a checkpoint (fmt/clippy/test) after each:**
  1. **S1, #578–#587 (9 PRs).** The most conflicts. Regenerate the pure moves (#578, #583, #584, #585, #586, #587) on the new base instead of resolving hunk by hunk. Apply the #581/#582 fixes here. Everything above depends on these paths.
  2. **S2, #588–#599 (11).** Moderate conflicts: Pass 1 API renames.
  3. **S3, #601–#605 (6).** #604 and #605 carry fixes; #604 is the worst conflict after S1.
  4. **S4, #607–#613 (6).** Shrink #608.
  5. **S5, #574, #620, #618, #621, #627 (5).** #620 fix.
  6. **S6, #625–#634 (4).** #625 decision.
  7. **S7, #616–#641 (14).** Mostly deletions: on conflict, take the delete.
- **Parallel behind the frontier:** once a segment is restacked, run its per-PR verification and independent reviews in background agents while the next segment is rebased.
- **Option needing a user decision:** S7 (legacy cleanup) depends little on S3–S6. Moving it to right after S1 would delete `replacement.rs` and the legacy search early and shrink the conflict surface for S2–S6. It would also let S7 run as a sibling of S2–S6. Either way, the chain order changes, so ask first.

Tooling notes from memory: git is 2.34, so there is no `--update-refs`; use the loop in the scratchpad's `restack.sh`/`restack2.sh`. Use `TMPDIR=/mydata/tmp-build`. Push backup refs `origin/backup/<branch>-before-…` before any force-push.
