# ASAPPlanner End-to-End Use Case: Inputs, Outputs, Missing Components, Adding a Summary Family

Status: proposal. Tracking issue: #685.

This document builds on [planner-layering.md](planner-layering.md) (#509: stages,
stage inputs table, Assumptions 1–8, pruning vs selection, Examples, Scenarios),
[asap-primitive-schema.md](asap-primitive-schema.md) (#573: summary coverage,
maintained populations) and [decoupling_op_and_expr.md](decoupling_op_and_expr.md)
(#511: operators vs scalar expressions). It does not repeat them; it collects,
for one deployment that wants to plan a workload end to end, what it must
supply, what it gets back, and what is still missing. Every "on main" claim
refers to `origin/main` at `6665fcc3`; everything else names its issue or PR,
or is marked **proposed; no issue yet**.

## 1. Purpose

A deployment (for example the ASAPQuery backend) hands ASAPPlanner a query
workload, a data workload and its deployment inputs, and gets back one selected
`PhysicalASAPDAG` for the whole workload, plus the candidate sets and reasons
that explain the choice.

- **The planner stays deployment-agnostic.** It plans one workload over named
  sources. Tenancy, dataset identity, the summary data store, storage engines
  and wire/persistence formats of stored state belong to the deployment, which
  composes them with the planner's output.
- **The planner never executes.** Stage 4 (execution) is the deployment's
  (planner-layering.md Assumption 8): it runs the selected plan as given and
  does not change summary choices or materialization. The planner does not
  measure execution either (Assumption 5); measurements reach it only as model
  inputs.
- **No execution fallback.** If no valid candidate exists, selection fails with
  reasons; the planner does not silently fall back to a different plan.

## 2. Inputs to ASAPPlanner

Entry point on main: `asap_planner::e2e_plan(UserInput)` (`crates/planner/src/lib.rs`).
`UserInput { workload: &PlanningWorkload, frontend_specific: FrontendInput, models: PlanningModels, pass }`.
Stage 0 lowers each query; the default pass `Stage1ToStage3Pass` then calls
`run_stage1_to_stage3(roots, targets, data, models)`
(`crates/asap-aware-mapping/src/pass/stage1_to_stage3.rs`).

The "stage" column follows #509's stage inputs table; "main" says whether the
pipeline on main actually reads it today.

### 2.1 Query workload (`PlanningWorkload.query_workload: QueryWorkload`, `crates/types/src/workload.rs`)

| Input | Stage (#509) | Type on main | Read by the pipeline on main? | Default when absent | Supplied by |
|---|---|---|---|---|---|
| Queries and language | 0 | `QueryWorkload.language: QueryLanguage` (`PromQL`, `MetricsQL`, `SQL(dialect)`, `DataFusion`); `query_batch: Vec<BatchEntry>`, `repeating_queries: Vec<RepeatingEntry>`, normalized by `entries()` into `QueryWorkloadEntry` | Yes (frontends) | none; an unsupported construct fails the whole call | user / dashboard owner |
| Frontend context | 0 | `FrontendInput::{Sql{catalog: SqlCatalog}, Promql{now_ms, histograms}, Metricsql{now_ms}}` | Yes | none (must match `language`) | deployment |
| Accuracy target | 1, 3 | `QueryRequirements.accuracy: AccuracyRequirement::{Explicit(AccuracyTarget), ImplicitExact}`; `AccuracyTarget::{Exact, Epsilon, EpsilonDelta}` | Yes: Pass 1 pruning and sizing, Stage 3 validity | `ImplicitExact` → `Exact`; `Epsilon` alone uses `DEFAULT_DELTA = 0.01` (`realization.rs`) | user |
| Latency requirement | 3 | `QueryRequirements.response_latency: LatencyRequirement::{ExplicitMaxMs, Unspecified}` | **No**: not passed to `run_stage1_to_stage3`; Stage 3 says "latency bounds ... are not checked yet" (#526, #660) | `Unspecified` | user |
| Recurrence / demand | 2 | `QueryRecurrence::{OneTime{invocations, execute_at}, Repeated(RepeatedDemand), Unknown}` | **No** on main; Stage 2 materialization uses it in #604 (G8), evaluation rate restored in G4 | `Unknown` | user or deployment (observed) |
| Predictability | 2 | `Predictability::{AdHoc, Predictable{known_at}, Unknown}` | **No** on main; #604 | `Unknown` | user |
| Time selection | 1 | `TimeSelection { scope: QueryTimeScope, lookback, as_of }` | **No** by the Stage 1→3 pipeline (window shape comes from the query text and `now_ms`); window segments in G10 (#628, #634) | `default()` (`Unknown`, none) | user |

### 2.2 Data workload (`PlanningWorkload.data_workload: Option<DataWorkload>`)

| Input | Stage (#509) | Type on main | Read on main? | Default when absent | Supplied by |
|---|---|---|---|---|---|
| Arrival (at rest vs streaming) | 2 | `DataArrival::{AtRest, ContinuouslyIngesting, Mixed, Unknown}` | No (#604 uses it for `MatAtIngestion`) | `Unknown` | deployment |
| Ingestion interval / volume / rate | 2, 3 | `Evidence<DurationMs>`, `Evidence<u64>`, `Evidence<Rate>` with `EvidenceSource` and freshness (`value_at`) | Rate and cardinality: Stage 3 edge statistics | `Evidence::default()` (no value) → Stage 3 defaults | deployment (declared or observed) |
| Input cardinality | 3 | `Evidence<u64>` | Yes, Stage 3 pricing | no value | deployment |
| Distribution | 3 | `Evidence<DataDistribution::{Zipf, Uniform, Bursty}>` | No by the Stage 1→3 pipeline | `Zipf` | deployment |
| Source schemas | 0 | `SqlCatalog` (SQL), PromQL series model; `HistogramCatalog` for classic histograms | Yes (frontends) | none for SQL; PromQL implicit | deployment |
| Raw-data retention | 2, 3 | not on main | — | — | #609 (G9) |
| Value domains, sign, counter declarations | 1, 3 | partly: `WeightDomain` on the summary input (`post_asap/sketch.rs`, proven by the planner from the expression); evidence hooks `quantile_input_domain`, `estimator_contract`, `topk_max_distinct_items` on `AccuracyEvidenceProvider` | `WeightDomain` yes (CMS admission in Stage 3); evidence hooks: default `None` | `UnknownOrSigned` / `None` | deployment; a declared-domain input type is **proposed; no issue yet** |
| Distinct-count / population bounds | 1, 3 | `EstimatorContract` via `estimator_contract`; `ClassicHllConfidence { max_distinct }` | Compiled, not wired into sizing or selection | `None` → no certificate | deployment; #678 |

### 2.3 Cost model

| Input | Stage | On main | Default | Status |
|---|---|---|---|---|
| Analytical prices | 3 | Stage 3 prices every node with `analytical_cost::estimate_operator` over edge statistics from `DataWorkload`; summaries as rows × depth (update) and groups × k (estimate); state size from `summary_shape` (`stage3_plan_selection.rs`). Charged once per computation × `NodeCost::multiplier`. | built-in; "illustrative, not calibrated" | — |
| `PlanningModels.cost: &dyn CostModel` | 3 | Field exists; **Stage 3 does not read it** (doc comment on `PlanningModels`) | `DefaultCostModel` | removal with the legacy search proposed in open PR #635 |
| Deployment prices and calibration | 3 | not on main | — | per-second cost, memory pricing and calibration: #594 (G4); exact-state and KLL δ pricing: #682 |
| Cost model 1 (usage over time), cost model 2 (peak capacity), cost model 3 (own-cluster trade-offs) | 3 | not on main | — | #683, #660 |
| Measured costs (sketch-bench) | 3 | `empirical_cost.rs`, `empirical_resources.rs` compiled, not called | — | #679; validation vs analytical: #347 |

### 2.4 Accuracy model

| Input | Stage | On main | Default |
|---|---|---|---|
| Built-in theoretical model | 1 | `PlanningModels.accuracy: &dyn AccuracyModel` (`local_guarantee`, `propagate`); per-family estimators in `accuracy/estimators/` (CMS, Count Sketch, HLL, KLL, DDSketch, UnivMon, cardinality). Stage 1 composes each root's guarantee with it. | `DefaultAccuracyModel` |
| Deployment empirical evidence | 3 | `PlanningModels.evidence: &dyn AccuracyEvidenceProvider::output_guarantee(&QueryRoot)`: read only in Stage 3; admits a candidate without a theoretical guarantee meeting the target when evidence shows the output meets it | `NoAccuracyEvidence` (all `None`) |
| Estimator contracts | 1, 3 | `estimator_contract`, `topk_max_distinct_items`, `quantile_input_domain`, `propagation_stats` | `None` |
| HLL confidence | 1, 3 | `ClassicHllConfidence` compiled, not wired | #678 |
| Composition through arithmetic/comparisons, top-k band | 1, 3 | not on main (guarantee `None` above such nodes) | #669 |

### 2.5 Deployment capabilities

| Input | Stage | On main | Status |
|---|---|---|---|
| Supported summary families and operators | 3 (prune earlier per planner-layering.md §Design Requirements) | Executor-side checks exist in `asap-physical-operators/src/capability.rs` (`validate_summary_kernel`, `validate_native_family`, ...); Stage 3 does **not** check capabilities ("deployment capabilities are not checked yet") | #609 (G9), #525 |
| Materialization media (memory, disk), `MatAtIngestion` / `MatAtQuery` support | 2 | Stage 2 emits only `NotMaterialized` | #604 (G8), #609 |
| Memory / disk budgets | 3 | not on main | #660 |
| Executor support for scalar operator references | 4 | not on main | #668 |

### 2.6 Other inputs

| Input | On main | Status |
|---|---|---|
| Optimization pass override | `UserInput::with_pass`, `PassRegistry` | on main |
| One bundle for all deployment inputs | separate `PlanningModels` fields and `DataWorkload` | #525 |
| Planning time `now_ms` (freshness of `Evidence`) | `FrontendInput::Promql{now_ms}`; `Evidence::value_at(now_ms)` | on main |

## 3. Outputs

| Output | On main | Consumer |
|---|---|---|
| `CandidateLogicalASAPDAGs` | `Stage1ToStage3Run.stage1_logical_asap_optimization`: one whole-workload candidate per choice of a per-computation candidate for every target (Cartesian product, every one enumerated); a candidate that cannot compose carries its error | stage_pipeline, tests |
| Pass 1 pruned candidates | each with a `PruneReason` (only provably invalid) | stage_pipeline |
| `CandidatePhysicalASAPDAGs` | `stage2_physical_asap_optimization`: one per composed logical candidate (all `NotMaterialized`); unimplementable ones listed under `pruned` with the error | stage_pipeline |
| Selection | `stage3_plan_selection: Result<Selection, SelectionError>`: the selected id; rejected candidates with reasons (accuracy miss, CMS without non-negative weights); valid-but-costlier candidates; per-node `NodeCost { cost, multiplier, detail }` and `CandidateCost.total` | stage_pipeline, deployment |
| Per-node guarantees | every node keeps the guarantee Stage 1 composed for it | deployment, user explanations |
| Selected plan for execution | `PlanOutput { plans, scalar_roots, scalar_plans }`; `PlanOutput::execution_timed_dag()` → `PhysicalASAPDAG`, wrapped in `PhysicalASAPDAGDocument { schema_version, dag }` with `validate()` (`PHYSICAL_ASAP_DAG_WIRE_VERSION = 10`, `crates/types/src/ir/physical_export.rs`) | deployment (Stage 4) |
| Inspection document | `stage_pipeline` (`crates/devtools/src/bin/stage_pipeline.rs`) writes `asap-stage-pipeline/v1` with the four stages; viewer redesign #574 (G11) | developers, reviewers |

Handoff today: the deployment calls `e2e_plan`, takes `execution_timed_dag()`,
validates the document, and executes it as given. The ASAPQuery backend plan
(backend restack) is to call the planner in-process, install the document, and
stop doing its own selection. Costs, guarantees and rejection reasons are not
part of the `PhysicalASAPDAGDocument`; only `stage_pipeline` exposes them.

## 4. Missing components for an end-to-end use case

### 4.1 Tracked

| Component | Status | Issue / PR |
|---|---|---|
| Early search-space pruning without discarding a possibly-best plan | open; every combination is enumerated and priced (planner-layering.md Assumption 7) | #673 |
| Plan search strategies (DP over nesting, MILP over shared DAGs) | open | #671 |
| DataFusion optimizer rules in SQL lowering; rule-list API | open | #670 |
| Filter / HAVING push-down across levels for more candidates | open | #664 |
| Pass 2 reuse rules: rollup, top-k reuse, nested sum merge | open | #680 |
| Pass 2 ASAP-aware CSE (identical expressions, summary capability, windows/panes, segments) | not on main ("Pass 2 is not planned yet") | G4 (#588, #589, #593, #594, #640), G5 (#595–#599, #621, #627), G6 (#591, #592, #601), G10 (#628, #632, #634) |
| Stage 2 materialization (`MatAtIngestion` / `MatAtQuery`, media) | open PR | #604, #606 (G8) |
| Guarantee composition through arithmetic and comparisons; top-k band | open | #669 |
| HLL confidence in sizing and selection | open | #678 |
| Measured costs; cost models 1/2/3; memory, disk, latency trade-offs | open | #679, #683, #660, #594, #682 |
| Deployment inputs and capabilities, raw-data retention | open PR | #609 (G9), #525, #610 |
| Latency bounds in selection; reasons for losing candidates | open | #526, #660 |
| Maintained-population top-k in Pass 1 | open | #681 |
| Top-k output schema keeps item columns | open | #672 |
| Executing scalar operator references in the physical DAG | open | #668 |
| Grouped approximate counts via Hydra in Pass 1 | open PR | #605 (G7) |
| Docs after consolidation | open | #677 |

### 4.2 Not yet tracked (proposed)

| Component | Why it is needed end to end | Status |
|---|---|---|
| **Plan handoff contract beyond the DAG.** A selected-plan envelope carrying, per root, the accuracy target and composed guarantee, per-node cost, the planner revision and a digest of the inputs/models used. Compatibility rules for `schema_version` bumps. | The deployment cannot today tell which guarantee a result carries, or whether an installed plan was made with stale inputs. Must stay format-agnostic (no tenancy or storage fields). | proposed; no issue yet |
| **Validation that the deployment can execute the selected plan.** Planner-side capability check (#609) plus a deployment-side dry bind of the document before installation; per-operator timing in `validate` is #656. | Assumption 8 needs the plan to be executable as given; today a failure surfaces only at bind time. | partly #609, #656; dry-bind step proposed; no issue yet |
| **Re-planning when the workload or data changes.** What triggers a re-plan (query added/removed, `Evidence` expired, cardinality drift, new deployment prices), and how the new plan's materialized state relates to the old one (keep, rebuild, backfill). | planner-layering.md "Adding a new query" says the whole workload is re-planned; the transition between installed plans is undefined. | proposed; no issue yet |
| **Runtime feedback into the models.** Observed error, cost and cardinality from Stage 4 flow back as `Evidence` / `output_guarantee` / calibration. | Assumption 5 makes models the only channel; nothing defines the loop. | #250 (error), #347 (cost validation); cost and cardinality loop proposed; no issue yet |
| **Latency estimation.** A per-candidate query-time latency estimate, needed before `LatencyRequirement` can be enforced. | Stage 3 has prices, not latencies. | partly #526, #660 |
| **User-facing explanations.** A stable, non-devtools summary of why a plan was chosen and why each alternative lost (accuracy miss, capability, cost). | `stage_pipeline` is for developers; a deployment UI needs a stable form. | proposed; no issue yet |
| **Declared value domains and counter semantics as data-workload inputs** (non-negative, bounded range, monotone counters with resets). | Required for CMS admission over signed inputs, DDSketch relative error, KLL/quantile domains, counter `rate`. | proposed; no issue yet |
| **End-to-end acceptance on a real deployment** (planner-layering.md Examples 1–4 executed by the backend and compared with the planner executor). | Proves the handoff works. | backend restack; planner-side issue proposed; no issue yet |

## 5. Adding a new summary family end to end

This extends planner-layering.md "Adding a new summary family" (Given 1–6) with
where each item lives in the code on main.

| Step | What to declare or implement | Where on main |
|---|---|---|
| (a) Capability declaration | Which `AggIntent` it answers (`Count`, `Quantile`, `TopK`, `Cardinality`, `FrequencyL2`, `FrequencyEntropy`, ...) and which `SketchStatistic` it reads out (`PointCount`, `Quantile`, `Cardinality`, `FrequencyL2`, `FrequencyEntropy`); input value types; weight domain (non-negative only?); grouping strategy (one state per group, or one state over all groups like Hydra); mergeability, subtractability, deletions (needed for Pass 2 sharing, panes, rollup); window support | `SketchAlgorithm` / `SketchParams` / `HydraKind` in `crates/types/src/post_asap/sketch.rs`; `WeightDomain`; Pass 2 sharing in G4–G6, G10 |
| (b) Pass 1 offer | Add it to `summary_candidates(intent)` so Pass 1 offers it, **unsized**, wherever the intent matches | `crates/asap-aware-mapping/src/realization.rs`; `per_computation_candidates.rs` |
| (c) Sizing and theoretical bound | Sizing formula in `default_size_params` (dispatches to `accuracy::estimators::size_params`), and the inverse error bound in `AccuracyModel::local_guarantee`. With no theoretical bound, the family is valid only where `output_guarantee` evidence shows the target is met | `realization.rs`, `accuracy/estimators/<family>.rs`, `accuracy/mod.rs` |
| (d) Stage 2 | Physical operator implementation; any rewrite the runtime needs (as for exact top-k) | `stage2_physical_asap_optimization.rs` |
| (e) Stage 3 | Update cost per row and state bytes in `summary_shape`; analytical pricing; later measured costs (#679) | `stage3_plan_selection.rs`, `analytical_cost.rs` |
| (f) Executor | Kernel that wraps `asap_sketchlib` (fix gaps upstream or by moving the pin, not with a local copy); build, merge, estimate semantics; capability checks; coverage (#573: definition + selection) so equal states are merged only over disjoint rows | `crates/asap-physical-operators` (`summary_kernels`, `capability.rs`), `crates/types/src/ir/summary_coverage.rs` |
| (g) Measurements | sketch-bench rows for empirical accuracy (accuracy metric per row) and cost (update, merge, query, bytes) at the canonical shapes | sketch-bench (outside this repo); feeds #679 and `output_guarantee` |
| (h) Tests | Pass 1 catalog (offered, unsized; pruned with reason where invalid); composition/guarantee; Stage 3 validity and cost; executor results vs exact within the bound; one acceptance example through `stage_pipeline` | `crates/asap-aware-mapping/tests/logical_candidates.rs`, `crates/integration-tests`, `crates/devtools` |

### 5.1 Matching workloads to summary capabilities

A family is **eligible** when its intent matches (Pass 1), **valid** when it can
meet the target under the declared inputs (pruned in Pass 1 if provably not,
rejected in Stage 3 otherwise), and **worth selecting** when Stage 3 prices it
cheapest for the whole workload.

| Workload fact | Effect | Example on main or in flight |
|---|---|---|
| Intent and statistic | Eligibility via `summary_candidates` | `Quantile` → KLL, DDSketch; `TopK` → CMS-with-heap, Count-Sketch-with-heap; multi-column `Cardinality` drops UnivMon |
| Accuracy target (ε, δ) | Sizing and validity; `Exact` admits only exact candidates | KLL sized for δ: #682 |
| Value sign / weight domain | CMS's one-sided bound assumes non-negative weights | Stage 3 rejects CMS unless `WeightDomain::NonNegative` is proven |
| Distinct-count bound | HLL confidence needs an enforced bound ≤ the certificate's `max_distinct` | `ClassicHllConfidence`; #678 |
| Value range / relative error | DDSketch's relative-error guarantee depends on the input domain contract (`quantile_input_domain`) | DDSketch domain gaps: #527 |
| Grouping and cardinality | Many groups: one state per group multiplies state; Hydra keeps one state over all groups at a collision cost (`hydra_shared_grid_collision_bound`) | Hydra in Pass 1: #605 |
| Windows, recurrence, arrival | Mergeable families allow panes, sharing and rollup; ingestion-time materialization needs streaming arrival and repeated demand | #604, G6, G10, #680 |
| Filters and HAVING | Measure filter becomes `SummaryAgg.filter`; value HAVING stays above with guarantee `None` | on main (Pass 1); push-down #664 |
| Retention and at-rest data | Query-time summaries over retained raw data vs stored state only | #609 |

## 6. Open questions

1. Should the selected-plan envelope (guarantees, costs, input digest) live in
   ASAPPlanner as a format-agnostic type, or be composed by each deployment from
   `Selection` and the DAG?
2. Should value domains and counter semantics be declared in `DataWorkload`, or
   only through `AccuracyEvidenceProvider` hooks as today?
3. Who triggers re-planning: the deployment on its own rules, or the planner
   from `Evidence` freshness? And may a re-plan keep existing materialized state?
4. Should capability pruning (#609) run in Pass 1 / Stage 2 to shrink the
   candidate set, or stay in Stage 3 so rejected candidates keep their reasons?
5. Is a family with no theoretical bound and no evidence offered at all in
   Pass 1, or only when the deployment declares evidence for it?
6. Which sketch-bench shapes are canonical for measured costs (#679), and how
   are they mapped to cost models 1/2/3 (#683)?
