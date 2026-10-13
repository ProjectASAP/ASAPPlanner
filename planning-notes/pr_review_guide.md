# Review guide: 80 open `zzylol` PRs in ProjectASAP/ASAPPlanner

Snapshot from 2026-10-12. Sources: the PR bodies and file lists, `chain2.txt`, and these design docs: #509 `planner-layering.md`, #511 `decoupling_op_and_expr.md`, #573 `asap-primitive-schema.md` (all on `main`) and `stage3-cost-model.md` (added by #594 in the stack).

I treat these as settled and do not raise them again:
- the planner has no execution fallback;
- the pass-through raw candidate stays an ordinary candidate;
- Stage 2 decides materialization, with no lifecycle stage;
- compatibility with old plans does not matter;
- the planner stays deployment-agnostic;
- the crate layout follows #572;
- Stage 1 uses no cost model;
- #660 sets the direction for cost models.

Many PRs also carry decision IDs you approved earlier: S1–S6, W1–W8, X1, C1–C5 and Q36–Q68. For those PRs, the questions below only ask two things: does the code match the decision, and does the design doc now say the same thing?

---

## 1. How to review

### Order: stack order, in groups

| Group | PRs, in stack order | Attention |
|---|---|---|
| **A. Phase A cutover** (under review now) | #540 → #541 → #542 → *[B]* → *[F]* → #543 → *[D]* | **High**, already in progress. B, F and D are new local PRs. #542 is being split into #542, B (pure move) and F (one physical DAG plus the coverage e2e tests). D takes the lifecycle removal and the query-time default out of #543. |
| **C. Stage pipeline MVP** | #561 → #575 → #576 → #577 → (#578) → #579 → #581 → #582 | **High** for #561, #575, #576 and #581. **Low** for #577, #579 and #582. #561 is under review now. |
| **L. #572 crate layout** | #578, #583 → #584 → #585 → #586 → #587 | **Skim.** Pure moves. Check only the "deviations" listed in #578 and #586. |
| **W1. Pass 2 sharing and Stage 3 cost** | #588 → #589 → #594 → #593 → #592 → #591 → #595 → #596 → #597 → #598 → #599 | **High** for #588, #594 and #595. **Medium** for #589, #592, #593 and #598. **Skim** #591, #596, #597 and #599. |
| **W2. Windows, Hydra, materialization** | #601 → #600 → #604 → #606 → #603 → #605 | **High** for #601 and #604. **Medium** for #600, #603 and #605. **Skim** #606. |
| **W3. Filters and deployment inputs** | #607 → #608 → #609 → #610 → #612 → #613 | **Medium** for #607, #608 and #609. **Skim** #610, #612 and #613. |
| **V. Viewer and post-demo fixes** | #574 → #620 → #618 → #621 → #627 | **Medium** for #620, #621 and #627. **Skim** #574 and #618 (viewer). |
| **X. Window follow-ups** | #625 → #628 → #632 → #634 | **Medium–High** for #625 (new IR flag) and #628. **Medium** for #632 and #634. |
| **K. Legacy cleanup** | #616 → #617 → #622 → #624 → #629 → #630 → #631 → #633 → #635 → #636 → #637 → #638 → #640 → #641 | **Skim.** Deletions, moves and test ports. Check the behaviours they drop against #623. #640 is a small Pass 1 fix. |
| **O. Off-stack** | #590 (sibling of #589, diverged), #642 (on `main`), #412 (on `main`, parked) | See the table. |
| **S. Superseded legacy chain** | #552–#569 (on `stack/528-09-accuracy`) | **None.** Later PRs ported or replaced each one (table). Close them as superseded, or leave them until you decide. |

### Reading tips
- Most of the +/− in #575, #576, #588, #589, #601, #604 and #625 is the regenerated `tools/dag-viewer/examples/planner-layering-example1.json` fixture. Leave it out of the diff: `git diff <base>...<head> -- . ':!tools/dag-viewer/examples/*.json'`. The table also gives the size without the fixture.
- The GitHub diff of #590 is not meaningful. Its branch is 76 commits behind and 76 ahead of `stack/509-e2-keyed-additive-topk`, so it shows 374 files, including the lifecycle deletion. Its own commits are only the Example 2–4 specs and tests (`287aebb8`, `d064b46a`).
- PR descriptions written before the #646 restack still say "trusted whole-source coverage (#570)": #575, #608 and the commit list of #542. Since #646, coverage is derived (#573 §4.2.2). Check the code, not the prose (thread T12).

### Approvable by skimming (no design content)
#574, #578, #583, #584, #585, #586, #587, #591, #597, #599, #606, #610, #612, #613, #618, #616, #617, #622, #624, #629, #630, #631, #633, #635, #636, #637, #638, #641, #642. #582 also qualifies, because it carries out the settled decision Q36.

---

## 2. All 80 PRs

Size is +additions/−deletions, then the number of files. "code" leaves out the example-fixture JSON. Position is group·stack index from `chain2.txt`.

| # | Title (short) | Position | Size | Class | Reason |
|---|---|---|---|---|---|
| 540 | PromQL/MetricsQL → unified IR | A·1 | +7,546/−3, 17 | Design | Stage 0 lowering contract (#511 §3.3). Instant-selector lookback is left as an open point. Under review now; local updates pending. |
| 541 | Timing pass, PhysicalASAPDAG export, executor compiler | A·2 | +8,456/−12, 30 | Design | Timing contract, export contract and validation rules. Under review now; local updates pending (fallback removal). |
| 542 | Plan one shared unified workload DAG | A·3 | +24,740/−15,158, 199 | Mixed | Port of `MajorPass` to the unified IR is mechanical. Coverage on planned states and the one-DAG export are design (moving to F). Under review now. |
| 543 | Remove legacy scaffolding, finish tooling migration | A·4 | +2,252/−36,006, 143 | Mixed | Deleting the old IR is mechanical. The lifecycle removal and the all-query-time default are design (moving to D). Under review now. |
| 561 | Pass 1 local alternatives (no timing, no cost) | C·5 | +427/−0, 4 | Design | Defines the Pass 1 inventory contract. Under review now. |
| 575 | Stage 1 whole-workload candidates + `stage_pipeline` | C·6 | +16,144/−6, 7 (code +686/−6) | Design | Composition rule (Cartesian product). Output naming of `FinalizeExactAccumulator`. |
| 576 | Stage 2 physical + Stage 3 selection (MVP) | C·7 | +31,992/−381, 6 (code +1,064/−15) | Design | First Stage 2 and Stage 3 rules: top-k → Sort+Limit, accuracy rejection, analytical pricing. |
| 577 | Example 1 acceptance spec + e2e tests | C·8 | +2,689/−0, 3 (code +1,086) | Mixed | Tests are mechanical. The spec doc's "ambiguities/deviations" section interprets #509. |
| 578 | `asap-types` modules by #511 section (B1) | L·9 | +1,283/−1,928, 215 | Mechanical | Module move. Read the listed deviations from #572 and the B0 blocker. |
| 579 | Top-k readouts return selected rows | C·10 | +744/−162, 5 (code +392/−98) | Design | Changes the IR output schema of `SummaryEstimate{TopK}`. Small. |
| 581 | Stage pipeline facade, tree-DP selection; retire MajorPass | C·11 | +1,591/−371, 18 | Design | Selection algorithm, its exactness guard, per-candidate rejection, exact top-k schema. |
| 582 | Stage 1 no longer depends on the cost model | C·12 | +5,303/−6,083, 35 | Mechanical | Carries out Q36. Moves the legacy selection and deletes cost hooks. |
| 583 | Extract `asap-logical-optimizer` (B2) | L·13 | +2,839/−2,488, 103 | Mechanical | Crate extraction. Legacy impls become free functions. |
| 584 | Extract `asap-physical-optimizer` (B3) | L·14 | +143/−15, 14 | Mechanical | Pure move. |
| 585 | Extract `asap-plan-selection` (B4) | L·15 | +346/−2,215, 63 | Mechanical | Move. Deletes three modules with no callers. |
| 586 | Pass facade into `asap-planner`; delete `asap-aware-mapping` | L·16 | +54/−115, 30 | Mechanical | Move. Note: `OptimizationPass` now depends on the frontends. |
| 587 | Rename to `asap-executor` (B5) | L·17 | +207/−226, 99 | Mechanical | Rename. |
| 588 | Pass 2 identical-expression rule | W1·18 | +52,014/−9,017, 12 (code +791/−298) | Design | Sharing as a workload-level variant. DP over variants. Unique key on the PromQL scan. |
| 589 | Whole-expression top-k heap over raw samples | W1·19 | +40,669/−15,188, 8 (code +598/−165) | Design | New Pass 1 template that absorbs the inner target (`absorbs`). DP change. |
| 594 | Stage 3 per-second cost, memory, calibration | W1·20 | +1,820/−1,047, 12 (code +925/−152) | Design | Cost model and design doc (S1/S2/S5). `RootDemand`. |
| 593 | Declare counter metrics (CMS non-negativity) | W1·21 | +1,906/−484, 15 (code +438/−144) | Design | New workload input `metric_types`. New proof variant (X1). |
| 592 | Relative coverage time, mergeable families | W1·22 | +276/−14, 4 | Design | IR contract: `family_merges` and `merge_relation` (#573 §5.6). |
| 591 | Executor: tumbling pane merges | W1·23 | +398/−1, 2 | Mechanical | Executor port of #566/#569. |
| 595 | Pass 2 summary-capability rule | W1·24 | +613/−72, 11 | Design | Strictest-consumer sizing (W5). `Sharing` enum. DP coupling fix. |
| 596 | One UnivMon for distinct/L2/entropy | W1·25 | +175/−6, 3 | Mixed | New `FrequencyMoments` capability key (design). The rest is the uncertified-UnivMon note. |
| 597 | Executor: exact distinct/L2/entropy, typed UnivMon ids | W1·26 | +776/−22, 14 | Mechanical | Port of #557/#559/#563. |
| 598 | SQL frequency L2/entropy idioms | W1·27 | +801/−2, 10 | Mixed | Port of #562/#564, but the rule now runs in the frontend and the original SQL DAG stops being a candidate. |
| 599 | Executor: exact SQL entropy fallback; Example 2 status | W1·28 | +468/−11, 9 | Mechanical | Port of #565, plus a status test. |
| 601 | Pass 2 window composition: tumbling panes | W2·29 | +163,852/−26,311, 18 (code +2,888/−307) | Design | Window form, pane width, merge timing and merge pricing (W1–W4). |
| 600 | Executor: HydraCms kernel | W2·30 | +1,023/−7, 12 | Mixed | Kernel and operator are executor work. The Hydra IR contract, grouping-aware `family_merges` and the mirrored sketchlib encoding are design. |
| 604 | Stage 2 materialization | W2·31 | +123,988/−33,194, 18 (code +1,824/−234) | Design | Materialization option space, eligibility (S4), selection, latency check (S6). |
| 606 | Example 4 acceptance with materialization | W2·32 | +525/−39, 4 | Mechanical | Test port and a devtool example. |
| 603 | SQL `COUNT(*)` counts rows | W2·33 | +241/−9, 6 | Mixed | Unit-weight update rule in Pass 1. `SummaryBuild.value` becomes `Option`. |
| 605 | Pass 1 offers HydraCms for grouped counts | W2·34 | +438/−37, 9 | Design | New candidate family, ε/2 sizing, grouping-aware guarantee (W7). |
| 607 | Executor: filtered aggregates and summary builds | W3·35 | +750/−119, 11 | Mixed | Mostly executor work. Per-measure filter semantics, IR nullability and the empty-KLL error are design (#511 §3.4). |
| 608 | Pass 1: single-measure filtered aggregates | W3·36 | +350/−42, 5 | Design | `SummaryAgg.filter` as a realization rule. Coverage of filtered states. |
| 609 | Deployment inputs: capabilities, raw retention | W3·37 | +989/−45, 18 | Design | `DeploymentCapabilities` API, Stage 3 capability checks, raw retention cost (C1–C5, Q48). |
| 610 | Show deployment inputs in stage doc/viewers | W3·38 | +196/−4, 5 | Mechanical | Devtool document section. |
| 612 | Example 4 B1 vs B2 as invariants | W3·39 | +111/−19, 2 | Mechanical | Test-only (Q49). |
| 613 | `stage_pipeline` examples 2 and 4b | W3·40 | +307/−39, 2 | Mechanical | Devtool only, including the devtool's own candidate-cap fix. |
| 574 | Viewer redesign around stage pipeline | V·41 | +8,833/−4,233, 23 (code +1,613/−4,233) | Mechanical | Viewer UI (approved redesign). |
| 620 | Pass 1: no per-group sketch for SQL `COUNT(*)` | V·42 | +240/−73, 4 | Design | Pass 1 stops generating some alternatives (Q55). |
| 618 | Viewer/devtool: SQL editor over declared tables | V·43 | +280/−41, 7 | Mechanical | Devtool and viewer (Q54). |
| 621 | UnivMon: certify L2 from layer-0 F2 | V·44 | +363/−52, 11 | Design | Accuracy contract, executor readout, sizing, pricing (Q64). |
| 627 | Stage 3: `satisfies` checks the error metric | V·45 | +84/−0, 3 | Design | `AccuracyModel::answers`, a contract and a table (Q65). |
| 625 | Stage 2: query time, kept (B3) | X·46 | +142,898/−54,445, 23 (code +612/−275) | Design | New IR flag `kept`, third materialization choice, pricing (Q56–Q59). |
| 628 | Shared year-aligned window segments | X·47 | +428/−81, 12 | Design | Segments stand in for EH. New `Sharing::WindowSegments` variant (Q60–Q62). |
| 632 | Price TimeShift free, TimeRange by kept rows | X·48 | +693/−614, 6 (code +113/−34) | Design | Cost rules (Q66). |
| 634 | Planner chooses the segment grid | X·49 | +393/−173, 9 | Design | Two segmentations, `WindowForm::Segments` encoding, a Stage 2 rule (Q67). |
| 616 | Delete `dag_export` bin | K·50 | +5/−3,214, 6 | Mechanical | Deletion (Q50). |
| 617 | Delete `dag_export` module | K·51 | +25/−1,863, 11 | Mechanical | Deletion. |
| 622 | Delete legacy physical-plan cost model | K·52 | +10/−1,820, 5 | Mechanical | Deletion. |
| 624 | Delete `show_post_asap_ir`; rename to `show_logical_dag` | K·53 | +17/−262, 5 | Mechanical | Devtools (Q51). |
| 629 | Tests select through `plan_stages` | K·54 | +248/−225, 15 | Mechanical | Test port (Q52). Some assertions dropped; listed in #623. |
| 630 | Delete legacy `candidate_selection` | K·55 | +18/−6,441, 9 | Mechanical | Deletion. Lost coverage listed in #623. |
| 631 | Move Stage 1 realization helpers out of legacy search | K·56 | +590/−580, 22 | Mechanical | Pure move (Q68). |
| 633 | Port legacy-search callers to Stage 1 | K·57 | +657/−1,296, 15 | Mechanical | Test and devtool ports. Redefines the `sketch_coverage` metric. |
| 635 | Delete legacy search and `CostModel` | K·58 | +103/−17,994, 31 | Mechanical | Deletion (Q68). Removes `PlanningModels.cost`, a public API (Q53). |
| 636 | Move `AccuracyModel` to plan-selection | K·59 | +199/−180, 12 | Mechanical | Trait move (Q53). |
| 637 | Docs: describe the stage pipeline | K·60 | +391/−912, 35 | Mechanical | Doc wording. Marks legacy docs historical. |
| 638 | Viewer: vertical lanes | K·61 | +13/−10, 3 | Mechanical | CSS. |
| 640 | Whole-expression top-k partitions on the heap input | K·62 | +120/−27, 2 | Mixed | Correctness fix to #589. One small rule: `without` keys switch the heap off. |
| 641 | Remove the unread accuracy-evidence API | K·63 | +30/−433, 10 | Mechanical | Removes dead code (`PlanningModels.evidence`). |
| 590 | Examples 2–4 acceptance specs + tests | O (on #589, diverged) | +179,540/−44,428, 374 (diff polluted) | Mixed | Specs interpret #509 Examples 2–4. The tests were already ported into #599, #601 and #606. |
| 642 | Layering MVP status report | O (on `main`) | +252/−0, 2 | Mechanical | Status doc. Stale after the B/F/D restack. |
| 412 | Multi-measure per-entity schemas; unsafe avg rewrite | O (on `main`, draft) | +324/−11, 7 | Design | Rewrite-safety rule on the legacy `QueryExpr`, which #543 deletes. Parked; leave open. |
| 552 | Executor: UnivMon states and readouts | S | +149/−3, 4 | Mechanical | Superseded: ported in #597. |
| 553 | Bundle capabilities with models | S | +69/−23, 6 | Design | Superseded by #609. Targets the deleted lifecycle. |
| 554 | Summary response latency bounds | S | +129/−4, 3 | Design | Superseded by the latency check in #604. Legacy lifecycle. |
| 555 | Compatible summary state merges | S | +299/−22, 7 | Design | Superseded by #560 (merged) and #592. |
| 556 | Response bounds on raw fallback | S | +143/−8, 4 | Design | Superseded by #604 (S6). Legacy lifecycle. |
| 557 | Executor: typed UnivMon identities | S | +194/−18, 5 | Mechanical | Superseded: ported in #597. |
| 559 | Executor: exact L2 and entropy | S | +269/−1, 6 | Mechanical | Superseded: ported in #597. |
| 562 | SQL L2 idiom recognition | S | +511/−2, 8 | Design | Superseded by #598. |
| 563 | Executor: exact distinct | S | +258/−68, 9 | Mechanical | Superseded: ported in #597. |
| 564 | SQL entropy idiom recognition | S | +454/−36, 6 | Design | Superseded by #598. |
| 565 | Executor: exact SQL entropy fallback | S | +249/−12, 10 | Mechanical | Superseded: ported in #599. |
| 566 | Window composition + materialization candidates | S | +524/−4, 6 | Design | Superseded by #591, #601 and #604. |
| 568 | Complete workload selection | S | +909/−7, 8 | Design | Superseded by #581 and #588 (reimplemented, W8). |
| 569 | Temporal pane merge timestamps | S | +85/−10, 3 | Mechanical | Superseded: ported in #591. |

**Counts: 36 Mechanical, 34 Design, 10 Mixed.** Of the 34 Design PRs, 9 are superseded or parked (#412, #553–#556, #562, #564, #566, #568). That leaves **25 active Design PRs and 10 Mixed PRs** to review for design content.

---

## 3. Design questions by area

Each question gives the doc section it implements or departs from, then what to look at. Where an approved decision covers the choice, the question only checks that the doc and the code agree.

### 3.1 Stage 0 frontends and the IR cutover (#540, #541, #542, #543; under review now)

**#540: PromQL/MetricsQL lowering**
1. Instant-selector lookback. The PR lowers `m{…}` to `TimeRange { range: data_ingestion_interval, kind: Instant }`. #511 §2.1 says "`TimeRangeKind::Instant` uses `range` as the lookback horizon … lookback is not the ingestion interval." The PR lists this as an open point. Decide now: use the Prometheus lookback delta (5 min default) as a workload/frontend input, or keep the ingestion interval and amend #511. Look at `frontend-promql/src/unified/` (selectors) and `lower_metricsql*`.
2. Where to put `HistogramCatalog`. It is a frontend-only input that decides exact versus sketchable `histogram_quantile`. #509 Assumption 4 says "Workload descriptions are inputs". Should histogram kind live in `DataWorkload`, as #593 later does for `metric_types`, or stay as a separate frontend argument? Look at `unified/histogram.rs` and thread T10.
3. Check against #511 §3.4: the listed gaps are rejected, not approximated. The PR says it rejects native histograms, dynamic `limitk`, fill modifiers and `histogram_count/sum/…`. Confirm the conformance tests cover each row of the §3.4 table.

**#541: timing and PhysicalASAPDAG**
1. Default timing. `LifecycleAssignment::default_maintained()` defaults to ingestion time, and D later flips it to query time. Since #509 Stage 2 owns the decision, should #541 already ship the query-time default and the `MaterializationAssignment` name, so D shrinks? Look at `ir/timing.rs`.
2. Validation rule. "No ingestion-time node reads a query-time value" matches #509 Algorithm 3 line 13: "everything upstream of an ingestion-time node also runs at ingestion time". `ConflictingTiming` plus `split_shared_by_phase` decide what happens when two consumers need different timings. #604 later returns the remapped assignment. Is erroring on conflict the intended contract for the IR? See #654.
3. Selection code in the executor. On `main` and still at #641's head, `executor/src/physical_planner/candidates.rs` has `enumerate_frontiers`, `compile_candidates` and `select_candidate`, which enumerate and select materialization frontiers in the executor. #509 §4 says the deployment "does not choose among summaries or decide what to materialize". Decide where to delete these: in #541's restack, or in a K-series cleanup. Keep `cut_candidate` and `frontier_from_timing`, which split a chosen plan. See thread T6.

**#542 / #543 (being split into B, F and D)**
1. #542 commit 2 "declares whole-source coverage … trusted (#570)". After #646, coverage is derived (#573 §4.2.2: "computes the coverage of a node from the sub-DAG the node covers, not from a declaration"). Confirm F's version derives coverage, and that the description is rewritten.
2. #543 and D: "Where lifecycle selection used to force raw recompute, the default cost model now picks sketches." That behaviour change is visible to users. Check that a test pins it, and that D owns it (not #543).

### 3.2 Stage 1, Pass 1: candidate generation

**#561: local alternatives (under review now)**
1. Sizing. #509 Pass 1 says "Summary nodes are created unsized … the summary is sized for the accuracy requirement later, since Pass 2 may tighten it". #561 sizes each `Realization::Sketch` with `default_size_params` in Pass 1, and #595 re-sizes for Pass 2. Accept "size in Pass 1, re-size in Pass 2" and update the doc, or keep sketches unsized until Pass 2? Look at `logical_candidates::local_realizations_for_intent`.
2. Invalid accuracy. A bad ε or δ is a hard `InvalidAccuracy` error for the whole inventory. #509 §Stages says "every pruned candidate carries a reason". Should it prune that target's summary alternatives with a reason and keep pass-through instead?
3. Multi-measure aggregates are skipped: they stay pass-through only. #608 also leaves the split as follow-up work. Confirm this is tracked, since the #509 Pass 1 table assumes per-computation targets.

**#575: whole-workload composition**
1. Exact accumulators are represented as `SummaryAgg → FinalizeExactAccumulator`, the exact state of #573 §5.3. This makes Example 1 give 24 candidates where #509 says 6. You accepted that in #579. Check that #509 Example 1 or the acceptance spec records why.
2. `FinalizeExactAccumulator` takes the replaced aggregate's output name. This is a schema contract that later PRs rely on (#601 fix 6). Should it be stated in #573 §5.3?

**#589: whole-expression heap**
1. A top-k alternative that absorbs its inner `sum_over_time` target replaces a larger sub-DAG than the target. #509 Algorithm 1 runs per eligible sub-DAG with independent replacements, so the Cartesian product (line 19) assumes the replacements do not interact. Accept `LocalLogicalTarget::absorbs`, where the absorbed target is forced to pass-through, as the general mechanism for nested targets? It is also the case that broke the DP (thread T4).
2. The weight and item choice reuse the legacy `realize_keyed_additive_summary_input`. #640 later had to remap the partitions. Is there a test for `by` keys absent from the heap input, which now switch the heap off?

**#593: counter declaration (X1)**
1. Placement. `DataWorkload.metric_types: BTreeMap<String, MetricType>`. #509 lists the data workload as "Arrival, volume, rate, cardinality and distribution". Metric type is a new kind of fact there. Confirm it belongs in `DataWorkload`, not a catalog shared with `HistogramCatalog` (#540). See thread T10.
2. Proof scope. `CounterSamples` holds only through `TimeRange` and plain `Sum`. Does `rate`/`increase` reuse the existing `ResetAwareCounterDerivative`, so the two proofs do not overlap?

**#603, #620: SQL `COUNT(*)`**
1. #620 (Q55) stops offering per-group CMS, CountSketch and UnivMon for SQL `COUNT(*)`, because they do not bind and add nothing. #509 §Stages: "Pruning removes an invalid candidate … every pruned candidate carries a reason", and capability rejection is a Stage 3 job ("needs a capability the deployment lacks"). Is "the rule does not match" the right framing, or should these be Stage 3 capability rejections from #609's set? Look at `logical_candidates::enumerate_local_logical_candidates`. See thread T7.
2. #603 makes `SummaryBuild.value: Option<usize>` and adds `unit_count_build`. #511 §3.2 says "`COUNT(*)` counts rows". Confirm `Constant(1.0)` with `UnitCount` is the general row-count update for all families, including Hydra in #605.

**#605: HydraCms (W7)**
1. Error basis. The collision term is relative to the whole input weight N, not one group's: `e·N·(1/shared_columns + 1/width)`. So a small group's count can be dominated by error, while the target is "read the same way". Decide what ε means for a grouped count: per group, or relative to the total. Look at `estimators::local_guarantee` (Hydra arm) and `hydra_guarantee`.
2. Scope. #509's Pass 1 table lists Hydra for `TopK(k, x) by (g)`, but only grouped `Count` is offered. The Example 1 test `stage1_q2_summary_families_are_heap_sketches_and_hydra` is still ignored. Confirm the doc says "TopK Hydra deferred" (W7), or that an issue tracks it.

**#607, #608: filtered aggregates (Q47)**
1. #511 §3.4 lists "General SQL aggregate `FILTER` …" as a gap: "Define per-measure semantics … A filter on the whole aggregate input is not a general replacement." #607 defines per-measure `filters` on `Aggregate` and a row `filter` on summary builds. Update #511 §3.4, which still says gap, and §3.2's `COUNT(x)` row, which says to lower via a 0/1 indicator; the PRs lower it as a filtered count.
2. Coverage of filtered states. The #608 description says "Coverage stays whole-source", but its e2e test expects selection `latency ∈ (−∞, 100)` with no filter left in the definition. That matches #573 §4.2.2. Confirm the code at head derives the `SummaryAgg.filter` into the selection, and fix the description.
3. Behaviour changes in IR typing: filtered `Quantile` and `FinalizeExactAccumulator` become nullable, and an empty-KLL quantile becomes an error instead of 0.0. Confirm these are wanted in the IR, not only in the SQL frontend.

### 3.3 Stage 1, Pass 2: sharing (#588, #595, #596, #598, #628, #634)

**#588: identical-expression rule**
1. Variant model. Stage 1 returns whole-workload *variants* (independent, all identical sub-DAGs merged), and selection runs the DP once per variant. #509 Algorithm 2 line 31 enumerates "each non-empty, non-conflicting subset O ⊆ Opts". This is the root of thread T1: decide here whether variants are the intended model.
2. `(series identity, ts)` becomes the unique key of the identified PromQL scan, so CSE's legality rule shares range selectors. This is an IR schema claim, "a series has at most one sample per timestamp". Is it true for all deployments (duplicates, out-of-order samples)? Look at `with_promql_series_identity`.

**#595: summary-capability rule (W5)**
1. One variant per workload. Mixed per-key decisions (key A shared, key B independent) are not listed, and the PR says so. The doc says "Applying a rule adds a shared candidate and keeps the independent candidates". Covered by W5 for within-key choices; the cross-key mixing is not. See thread T1.
2. Equality of "summary input data": same input sub-DAG including `TimeRange`/`WHERE`, same grouping, same quantile column. #509 defines summary input data as excluding the window, which the window-composition rule compares separately. Check the key does not include the window in a way that blocks the window rule.

**#596**: the `FrequencyMoments` key groups Cardinality over at most one column with L2 and entropy. Should an HLL for distinct and a UnivMon for L2 be allowed in one key? The PR re-sizes the others for the strictest ε. Low priority while F0 and entropy stay uncertified (Q45, Q64).

**#598: SQL idioms**
1. The recognition moved into the SQL frontend, and "the original relational DAG is no longer kept as a separate candidate". #509 §0's output is `CandidateLogicalDAGs (a set of LogicalDAG)`, so a frontend may emit both forms. Emit both (original and recognized), or accept that the exact reducer stands in for the original? Look at `frontend-sql/src/frequency.rs` and `lower_sql_dialect`.
2. Units and NULL. Entropy converts bits to nats, and an exact population count decides SQL's empty-input NULL. Confirm this `Join(Cross)` with an exact `Count` is acceptable in cost terms. It adds an exact pass over the input to every approximate plan.

**#628, #634: shared segments (Q60–Q62, Q67)**
1. #509 Example 3 Pattern A and Algorithm 2's `WINDOW_SUMMARIES` call for "one EH covering the oldest data any q in G reads". These PRs use exact year-aligned segments as a stand-in ("true EH later"). The doc needs a note, or #509 needs a segments row in the window-composition table.
2. All-shared only (Q62): the rule emits one `Sharing::WindowSegments` variant per segmentation. The doc expects pairwise and partial groupings (the ignored test `stage1_a_window_composition_groups_every_pair`). Same question as thread T1.
3. #634's Stage 2 rule: a merge over panes of different widths is "not a stream of panes", so it is never maintained or kept. That makes the ×4 grid never maintainable. Confirm Stage 2 should restrict this, rather than Stage 3 pricing it.
4. Encoding. `WindowForm::Segments { unit_ms, ends }` uses a bitmask so `WindowForm` stays `Copy`, which caps segments at the mask width. Is the 64-pane limit (also `MAX_PANES`) a documented pruning rule with a reason?

### 3.4 Window composition (#601)
1. Window form as a Pass 1 axis (W4). #509 says "Pass 1 creates no merge nodes: each candidate summarizes exactly its own window, and merging across windows comes from the window-composition rule in Pass 2". #601 puts `WindowForm::Tumbling` on Pass 1 alternatives, which creates `SummaryMerge` in Pass 1. Update the doc's Pass 1/Pass 2 split, or move the axis under `pass2/`? The file is `pass2/window_composition.rs`, but the axis is set on `LocalLogicalTarget::windows`.
2. Pane width (W3). Only `gcd(lookback, cadence)` is offered. #509 offers "`Tumbling(L)` for every L that divides every W_q and every E_q", and `Sliding`, and EH. Sliding was dropped (Q63) and EH replaced (Q60). Confirm the doc records both, or tracks them as deferred.
3. Caps. A form needs 2 to 64 panes. Outside that range the form is silently not offered. Should that be a pruning reason (#509 §Stages)?
4. Pricing. A merge costs one operation per input state, and its output is the union of the input groups. Should this pricing rule be in `stage3-cost-model.md`, where merges are not mentioned?

### 3.5 Stage 2: physical candidates and materialization (#576, #604, #625)

**#576 (MVP)**
1. A `MaintainPopulation` makes the whole Stage 2 candidate fail, "because it runs only at ingestion time". Is that a provable invalidity (pruning with a reason), or should Stage 2 give it an ingestion-time assignment now that #604 exists? `reject_maintained_populations` still exists at #641, and #641 flags `MaintainedPopulationStrategy` as unreachable. Decide whether to delete maintained populations or plan them.

**#604: materialization (S1–S6, Q44)**
1. Pruning by workload. #509 Algorithm 3 says "`recurrence`, `predictability` and the `DataWorkload` do not prune options here; they determine the cost". #604's eligibility (S4) prunes ingestion time by arrival, recurrence, predictability and window shape. #509 Example 4 itself says "Data `at_rest`: A2 is not generated". The doc contradicts itself, and S4 picked pruning. Fix Algorithm 3's sentence and list the eligibility rules in #509 Stage 2.
2. Option space. Units are `SummaryAgg` states only, not "each sub-DAG". The medium is memory only (S3; Algorithm 3 has `for each medium in {memory, disk}`). Query-time materialization for one batch (Example 4 A1) and per-consumer "not materialized" (A3, Q44) are not generated. Record these as current limits in #509 Stage 2 or in an issue, so the doc does not claim them.
3. Optimality. Above 16 down-closed sets per logical candidate, Stage 2 adds units greedily by Stage 3's cost and flags the result as not guaranteed optimal. That means Stage 2 consults Stage 3's cost. Is that consistent with "only stage 3 … discards valid candidates" (#509 §Stages)? See thread T4.
4. Latency check (S6). `latency_ms_per_cost_unit` turns cost units into milliseconds. Should this be a deployment calibration value, which it is in `Stage3Calibration`, with the assumption "one core and no queueing" stated in #509 §3?

**#625: B3, "query time, kept" (Q56–Q59)**
1. IR. `PhysicalASAPDAGNode.kept: bool` sits beside the two-valued `ExecutionTiming` (Q58). #509 Algorithm 3 has `MatAtQuery(medium)` as a general option. Is `kept` the representation of every `MatAtQuery`? A1 would need "kept within one batch" but not across evaluations. Write the mapping `NotMaterialized`/`MatAtIngestion`/`MatAtQuery` → (`timing`, `kept`) into #509 Stage 2 or #573.
2. Eligibility. Only tumbling panes whose roots repeat at an interval equal to the pane width can be kept. Confirm this is the intended B3 rule, and that kept units need no down-closure (the PR says they are unconstrained).
3. Capability gate. `query_time_retention: false` in the executor rejects every B3 candidate (Q56/Q57). This is consistent with #609. Confirm the Stage 3 reason text matches the doc's.

### 3.6 Stage 3: cost, selection and accuracy (#579, #581, #594, #609, #621, #627, #632, #592, #600)

**#581: tree-DP selection (Q38–Q41)**
1. #509 Assumption 7 says the planner "generates all possible candidates and then select[s]". The DP builds only single-target and pairwise candidates, and falls back to enumeration (≤ 64) when its exactness guard fails. Otherwise it returns `TreeDpNotGuaranteedOptimal`. Should a non-guaranteed result be allowed out of the facade, or be an error or warning in `PlanOutput`? Add the DP and its guard to #509 §3 or to `stage3-cost-model.md` ("Interaction with selection" covers additivity only).
2. `SelectionError` now has only `NoValidCandidate`, and each candidate's failure becomes a rejection reason. This matches "explain why a query has none". Check that `NoValidCandidate` carries the per-candidate reasons, not only a count.

**#579**: the top-k readout now returns the selected rows (partition keys + item identity + `value`). #573 §5.2 already mentions the change. #511 §3.3 also says "heavy-hitter `AggIntent::TopK` is not a substitute for sample-value ranking". Confirm that exact `Sort+Limit` and the sketch readout are only compared when the query is a heavy-hitter top-k.

**#594: per-second cost model (S1, S2, S5)**
1. Shape. Cost is one scalar ("cost per second of wall time"), and memory enters through `w_mem` at 1 GB ≈ 1/8 vCPU. #660 points toward memory, disk and latency trade-offs per candidate. Should the doc frame the scalar as "cost model 1", with a pluggable interface, so #660 can add another model without rewriting Stage 3?
2. Deployment cost model. #509 Assumption 5: "It uses the deployment's cost and accuracy models". After #635 removes `PlanningModels.cost`, the deployment supplies only `Stage3Calibration` (coefficients). Accuracy stays a trait (`AccuracyModel`, #636). Decide whether cost should also be a trait, or whether calibration is enough until #660. Thread T5.
3. `RootDemand { accuracy, recurrence, predictability, latency_ms }` sits in `asap_types::workload` and is passed to Stage 1 (for cadence, #601) and Stage 3. Check that it is derived from `QueryWorkload` entries in one place.
4. Defaults. The default statistics (1,000 series, λ = 1,000, 100 groups) "order candidates plausibly, but … are not measured". With #593, #604, #609 and #612, several example outcomes depend on these defaults. Should the doc state which example conclusions depend on them?

**#632: TimeShift free, TimeRange by kept rows (Q66)**
1. The PR justifies a free shift by how the reference executor works ("folds the offset into the selector's window bounds"). The planner is deployment-agnostic, so is this pricing rule a deployment calibration, or a property of the IR (seekable time order)? State the assumption in `stage3-cost-model.md` as a planner assumption, not as executor behaviour.

**#609: deployment inputs (C1–C5, Q48)**
1. Granularity. `summaries` lists family × layout × readouts. Exact operators that do not bind, such as the SQL window SUM, Int64 `SUM(c*c)` and keyed builds with `Constant(1.0)`, are not covered, so Stage 3 can still price unbindable plans (#620's invariant test found one). Should capabilities also list non-summary operators, or is the "every priced plan binds" invariant the backstop?
2. Placement. `asap_types::deployment::DeploymentCapabilities` is data only, and `asap_executor::capabilities()` is one deployment's set. This fits "deployment-agnostic". Replace #509's "TODO: define this data structure, issue #525" with a pointer to it.
3. Raw retention (Q48) is charged on the scan node as memory. With `raw_data_retained = false`, should the memory budget check also count raw retention? The doc says yes ("plus, when charged, this raw retention"). Confirm the code does.

**Accuracy (#621, #627, #592, #600)**
1. #627 (Q65): the `answers(statistic, guarantee)` table. Cardinality accepts `RelativeValue`, and TopK accepts `TopKMembership`. Should the table live in #509 §3 or `accuracy-models.md` as the contract deployments override?
2. #621 (Q64): `FrequencyL2` now reads `l2_sketch_layers[0].get_l2()` (layer-0 AMS), instead of `calc_l2`. That changes what the executor's UnivMon L2 readout returns. The `use-asap-sketchlib-impls` rule allows it, since the call is sketchlib's own API. Pass 1 still emits shapes beyond the hash-bit budget, which Stage 3 always rejects. Generate and reject, or cap in sizing?
3. #592: `family_merges()` fails closed for KMV/Theta (the PR asks), and `CmsWithHeap`/`CountSketchWithHeap` merges are rejected (W6). #560 dropped its heap-merge rejection on 2026-10-07. Confirm #592 is the agreed single place for family merge rules, with #573's footnote: "each family says how its inputs may overlap, §5.6". Also update #573 §5.6 "Status", which still says "#646 (open)".
4. #600: the Hydra group count "mirrors [sketchlib's] private subkey encoding", and drift is guarded only by kernel tests. That conflicts with the rule to use `asap_sketchlib` implementations and fix gaps upstream. Propose a sketchlib group-mass API, or move the pin. The weight cap `i32::MAX` and per-row whole-grid memory accounting are kernel limits to track.

### 3.7 Acceptance specs and status docs (#577, #590, #642)
1. #577 and #590 add `planner-layering-example{1..4}-acceptance.md` under `design_docs/proposals`, each with a section on "the places where the doc is ambiguous or conflicts". Decide whether these specs are design docs, which would resolve the ambiguities back into #509, or test fixtures that should move to `develop_docs`. #590's branch has diverged, and its tests are already in #599, #601 and #606. Rebase #590 for the specs only, or close it.
2. #642 lists the 67-PR stack as of 2026-10-05. After B, F and D and the fallback removal, it needs a refresh before merging, or should land after the stack.

### 3.8 Legacy cleanup with design side effects (#629, #630, #633, #635, #641)
These are mechanical, but each removes behaviour without a Stage 1 replacement. Check that #623 lists everything dropped: the avg → sum/count rewrite, rollup, exact composition, top-k reuse, CSE share-versus-recompute by recurrence, accuracy propagation, the evidence-ranked cost, the redefined `sketch_coverage` metric (#633), and `PlanningModels.cost`/`evidence` (#635, #641). One decision: is it acceptable to delete these before #580 parity? Approved under Q52/Q68; confirm the issue is complete.

---

## 4. Cross-PR design threads

| # | Thread | PRs | Decide in |
|---|---|---|---|
| T1 | **How sharing options are enumerated.** #509 Algorithm 2 line 31: "for each non-empty, non-conflicting subset O ⊆ Opts". The code has whole-workload variants (`Sharing::{Independent, IdenticalExpressions, SummaryCapability, WindowSegments}`), all-or-nothing per key (W5, Q62), and no mixing across keys or rules. Either amend Algorithm 2 or schedule subset enumeration. | #588, #595, #596, #628, #634 | **#588** (introduces variants); #595 introduces the `Sharing` enum. |
| T2 | **Pass 1 versus Pass 2 boundary for windows.** "Pass 1 creates no merge nodes" against `WindowForm` as a Pass 1 axis (W4). Pane width gcd only (W3), no sliding (Q63), segments instead of EH (Q60). | #601, #628, #634, #625 | **#601**; one doc edit to #509 Pass 1/Pass 2 and Example 3. |
| T3 | **Materialization option space versus #509 Algorithm 3.** Pruning by workload (S4) against "do not prune options here"; summaries only, not arbitrary sub-DAGs; memory only (S3); A1/A3 not generated (Q44, Q61); `kept` as the encoding of `MatAtQuery` (Q58). | #541, D, #604, #625, #634 | **#604**, with the IR mapping fixed in **#625**. |
| T4 | **Optimality guarantees.** Tree DP with an exactness guard and a flagged result (#581). Stage 2 greedy above 16 sets, using Stage 3 cost (#604). Assumption 7 says enumerate everything. Decide whether a flagged non-optimal plan may be returned, and document the DP. | #581, #589, #595, #601, #604 | **#581** |
| T5 | **The deployment's cost model input.** `PlanningModels.cost` was unused (#581) and removed (#635); the deployment now supplies `Stage3Calibration`. Accuracy stays a trait (#636). #660 wants trade-off models. Decide whether cost becomes a trait now or with #660. | #581, #594, #609, #635, #636 | **#594** (the cost doc) |
| T6 | **Selection code still in the executor.** `enumerate_frontiers`, `compile_candidates` and `select_candidate` in `executor/physical_planner/candidates.rs`, against #509 §4: the deployment "does not choose among summaries or decide what to materialize". | #541, #587, #604 (uses `cut_candidate`) | **#541** restack, or a new K-series cleanup PR |
| T7 | **Where unrunnable or useless candidates are removed.** Pass 1 not generating them (#620), Stage 2 hard failure (#576, `MaintainPopulation`), Stage 3 capability rejection (#609), silent caps (2–64 panes in #601, the 64-segment mask in #634, sketch shapes past the bit budget in #621). #509: only provably invalid candidates are pruned, "every pruned candidate carries a reason", and capability checks belong to Stage 3. | #576, #601, #609, #620, #621, #634 | **#609** (the capability model), with #620 as the test case |
| T8 | **Accuracy contract.** Which readouts are certified (UnivMon L2 only), metric matching (#627), Hydra error basis (#605), CMS non-negativity proofs (#593), merge soundness per family (#592, #600), strictest-consumer sizing (#595). #509 §3: "Accuracy is estimated by the deployment's accuracy model, not assumed from a summary's nominal bound". The built-in model is analytical; say so in the doc. | #592, #593, #595, #600, #605, #621, #627, #636 | **#627** for the contract; **#592** for merge families |
| T9 | **Use of sketchlib.** #600 mirrors a private encoding; #621 changes a readout. The rule is to use `asap_sketchlib` and fix gaps upstream or by moving the pin. | #600, #621, #597 | **#600** |
| T10 | **Workload and deployment input shapes.** `HistogramCatalog` (frontend argument, #540), `metric_types` (in `DataWorkload`, #593), `RootDemand` (in `asap_types::workload`, #594), `DeploymentCapabilities` (in `asap_types::deployment`, #609), and `PlanningModels` contents after #635/#641 (accuracy, calibration, capabilities). Settle one place per kind of fact: data facts, query requirements, deployment facts. | #540, #593, #594, #609, #635, #641 | **#609** (and fill #525) |
| T11 | **Top-k schema and semantics.** Selected-rows readout (#579), the same schema for exact `Aggregate{TopK}` (#581), Stage 2's exact top-k still carrying `ts` (gap noted in #589), partition remap (#640). #511 §3.3 keeps sample-value `topk` separate from heavy-hitter `TopK`. | #579, #581, #589, #640 | **#579** |
| T12 | **Coverage after #646.** Descriptions in #542, #575 and #608 still say "trusted whole-source (#570)". Code should use `SummaryCoverage::derive` everywhere: panes (#601, #634), filters (#608), Hydra (#605), segments (#628). #573 §5.6 status is out of date. | #542/F, #575, #601, #608, #628, #634 | **F** (coverage e2e tests), then fix descriptions at the restack |
| T13 | **Frontend recognition versus candidate generation.** SQL idioms recognized in the frontend, with the original DAG dropped (#598). Should Stage 0 emit several `LogicalDAG`s (#509 §0 output `CandidateLogicalDAGs`)? The avg → sum/count rewrite was removed (#635), and #412's overflow finding says the rewrite is unsafe anyway. | #540, #598, #412, #635 | **#598** |
| T14 | **Status of acceptance specs.** Design docs or test fixtures (#577, #590)? Their "doc ambiguities" sections should go back into #509. | #577, #590, #642 | **#577** |
| T15 | **Superseded legacy chain.** #552–#569 were ported or replaced (W8). Close them, or keep them as "superseded by" pointers. | #552–#569 | Author decision; no review needed |
