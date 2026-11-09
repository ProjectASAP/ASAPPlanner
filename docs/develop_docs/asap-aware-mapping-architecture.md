# ASAP-Aware Mapping architecture

Audience: developers changing the planner. This document maps the planning
stages of the [design](../design_docs/proposals/planner-layering.md) to the
code. For the public types and their rules, read the
[contracts](asap-aware-mapping-contracts.md); to add behavior, follow
[Extend ASAP-aware mapping](extend-asap-aware-mapping.md).

## Crates

One crate per planning stage. A stage crate depends only on `asap-types` and
the shared models (`asap-accuracy`, `asap-cost`), never on another stage crate
or on the executor: the types one stage hands the next are in
`asap_types::stages`, and the planner facade calls each stage in turn. A test
in `asap-integration-tests` (`crate_graph`) checks these edges with
`cargo metadata`.

| Crate | Role |
| --- | --- |
| `asap-types` (`crates/types`) | Shared vocabulary, by IR section: `ir::operator` (`OperatorNode`, `NonASAPOp`, `ASAPOp`, operator parameters, `AggIntent`), `ir::scalar` (`ScalarExpr`, `ColumnRef`), `ir::schema` (`Schema`, `FieldDataType`, summary-state types such as `SketchAlgorithm` and `SummaryTypeCapability`), `ir::properties` (`AccuracyTarget`, `AccuracyMetric`, `AccuracyGuarantee`, execution timing, summary coverage), plus `ir::flat` and `ir::physical_export`; the planner inputs (`workload`); Stage 2 decision data (`physical`: `PhysicalProperties` (timing and `Lifetime`), tumbling windows) |
| `asap-frontend-sql`, `asap-frontend-promql`, `asap-frontend-metricsql` (with `asap-frontend-common`) | Stage 0, the query-language frontends: lower each query into the logical DAG (`QueryRoot`s of `NonASAPOp` nodes, no summaries, no timing) |
| `asap-stage0-memo` (`crates/stage0-memo`) | Stage 0 hash-consing: the memo groups of the workload's query roots (`memo_of_queries`) |
| `asap-stage1-pass1-per-computation-candidates` (`crates/stage1-pass1-per-computation-candidates`) | Stage 1, Pass 1: the initial candidates of each memo group |
| `asap-stage1-pass2-reuse-candidates` (`crates/stage1-pass2-reuse-candidates`) | Stage 1, Pass 2: the reuse rules over memo groups and the logical edges |
| `asap-accuracy` (`crates/accuracy`) | The accuracy model the stages share: the analytical rules (per-algorithm estimators, composition, budget split, target check), the deployment's measured accuracy (`AccuracyEvidence`) and α^log (`producer_satisfies_logical_requirements`); depends only on `asap-types` |
| `asap-stage2-physical-optimization` (`crates/stage2-physical-optimization`) | Stage 2: materialization and physical operator implementation |
| `asap-cost` (`crates/cost`) | The cost model the stages share: resource-usage estimation (the analytical per-operator model, measured operation costs, state byte sizes) and the cost models (`ResourceUsage`, `CostModel`, `CostInputs`); depends only on `asap-types` |
| `asap-stage3-plan-selection` (`crates/stage3-plan-selection`) | Stage 3: `select_plan` over Stage 2's output; accuracy and latency checks, costing candidates with `asap-cost` |
| `asap-planner` (`crates/planner`) | The facade: `e2e_plan` runs Stage 0, then the optimization pass (`Stage1ToStage3Pass` by default), which orchestrates Stages 1–3 (`run_planning`); the planning models (`PlanningModels`) and the `OptimizationPass` trait |
| `asap-executor` (`crates/executor`) | Stage 4, the reference executor: operators and summary kernels over `asap_sketchlib`; it runs a `PhysicalASAPDAG` |
| `asap-devtools` | `asap_planner` and the other inspection tools, the design-doc example workloads (`planner_layering_examples`) and the candidate labels |

```text
asap-types ◄── frontends (Stage 0)
asap-types ◄── asap-cost (shared cost model) ◄── asap-stage2-physical-optimization, asap-stage3-plan-selection
asap-types ◄── asap-accuracy (shared accuracy model) ◄── asap-stage1-pass1-per-computation-candidates, asap-stage2-physical-optimization, asap-stage3-plan-selection
asap-types ◄── asap-stage0-memo (Stage 0 hash-consing) ◄── asap-planner
asap-types ◄── asap-stage1-pass1-per-computation-candidates (Stage 1, Pass 1) ◄── asap-planner
asap-types ◄── asap-stage1-pass2-reuse-candidates (Stage 1, Pass 2) ◄── asap-planner
asap-types ◄── asap-stage2-physical-optimization (Stage 2) ◄── asap-planner
asap-stage3-plan-selection (Stage 3) ◄── asap-planner (also depends on the frontends)
asap-types ◄── asap-executor (Stage 4; no stage crate depends on it)
```

## The stages

```text
QueryRoots (Stage 0)
  │  asap_stage0_memo::memo_of_queries → Memo (groups, no candidates), then
  │  asap_stage1_pass1_per_computation_candidates::pass1_memo
  ▼                                                                     Stage 1, Pass 1
Memo (each group's initial candidates; a summary state group one per θ)
  │  asap_stage1_pass2_reuse_candidates::generate_logical_asap_memo_candidates
  │    rules (rules/), required accuracy, dead candidates, chain groups,
  │    logical edges with α^log                                         Stage 1, Pass 2
  ▼
LogicalASAPMemoCandidates (memo, edges, pruned with root causes)
  │  asap_stage2_physical_optimization::generate_physical_asap_memo_candidates  Stage 2
  ▼
PhysicalASAPMemoCandidates (one candidate per valid (timing, lifetime), edges with α, pruned_candidates)
  │  asap_stage3_plan_selection::select_plan                                 Stage 3
  ▼
(PhysicalASAPDAG, Selection) — the plan, and accepted_accuracy_per_query,
pruned_candidates and the selection MILP's size and time
```

`asap_planner::run_planning` runs all of it over lowered roots: Pass 1, then
Pass 2, Stage 2 and Stage 3, which builds the selected plan.
It returns every stage's output (`PlanningRun`); `Stage1ToStage3Pass` is the
same pipeline behind the `OptimizationPass` trait.

| Concern | Where it lives |
| --- | --- |
| Pass 1: which computations a query has (`ComputationTarget`), how each can be computed (`HowToComputeAggregateMeasure`: `Exact`, `ExactAccumulator`, `Summary`), fused computations (`FusedComputation`); a summary the deployment does not support is not offered | `asap-stage1-pass1-per-computation-candidates`, `per_computation_candidates.rs` |
| Which summary families serve an intent (`summary_candidates`) | `asap-stage1-pass1-per-computation-candidates`, `realization.rs` |
| Pass 2: rollup from the finest common group, exact top-k reuse, each readout's required accuracy, dead-candidate elimination with root causes, chain groups and the logical edges | `asap-stage1-pass2-reuse-candidates`: `rules/{rollup,topk,chains}.rs`, `requirements.rs`, `dead_candidates.rs`, `logical_memo_candidates.rs` |
| What each summary type can merge and answer (`SummaryTypeCapability::of`: `merge_inputs`, `readouts`) | `asap-types`, `ir/schema/summary_type_capability.rs` |
| Stage 2: physical properties options per summary build (not materialized; materialized at ingestion time `AcrossEvaluations`; materialized at query time `OneEvaluation` or `AcrossEvaluations`), ingestion-time eligibility, physical operator implementation | `asap-stage2-physical-optimization` |
| Timing and data-state edge rules applied from a `PhysicalPropertiesAssignment` | `asap-types`, `ir/properties/timing.rs` and `ir/properties/execution_data_state.rs`; `PhysicalProperties` and `Lifetime` in `physical/properties.rs` |
| Stage 3: the selection MILP over execution contexts (constraint (K) for accuracy, (R) for retention), the plan it builds, resource usage (`ResourceUsage`), cost models (`CostModel`; the MILP implements cost model 1), measured operation costs | `asap-stage3-plan-selection`: `select.rs`, `cost.rs`, `milp/`, `plan.rs`; `asap-cost`, `lib.rs` and `measured.rs` |
| The analytical per-operator resource model Stage 3 measures with | `asap-cost`, `analytical.rs` and `operator_statistics.rs` |
| The accuracy model: each summary type's guarantee at its θ (`theoretical_guarantee`, or the deployment's `AccuracyEvidence`), its sign condition, and $\alpha^{\log}$ (`producer_satisfies_logical_requirements`) | `asap-accuracy` |
| What the executor supports (`DeploymentCapabilities`) | `asap-types`, `deployment_capabilities.rs`; Pass 1 receives only `supported_summaries` and Stage 2 only `query_time_results_retained` and `raw_data_retained` (`bool` arguments, so Stage 2 does not depend on the Stage 1 crate); Stage 3 reads none of it |
| The inputs every stage reads (`PlanningModels`: accuracy evidence, cost inputs, deployment capabilities) and the `OptimizationPass` trait | `asap-planner`, `pass.rs`; the facade gives Pass 2 the evidence and Stage 3 only `CostInputs` |

## Algorithm in one pass

1. **Stage 0** hash-conses the frontends' roots into memo groups.
2. **Pass 1** gives each aggregate computation one exact option, the
   summary types the deployment supports (a state group with one candidate
   per θ of the type's list, and a readout group), and fused computations.
3. **Pass 2** adds rollup and top-k reuse candidates, passes each query's
   accuracy target down to the readouts' required accuracy, eliminates the
   candidates no logical edge can use (recording each with its root
   causes), adds chain groups for the surviving state groups, and lists the
   logical edges with $\alpha^{\log}$.
4. **Stage 2** expands each logical candidate into one physical candidate
   per valid (timing, lifetime), lists the physical edges with
   $\alpha^{\mathrm{phys}}$, and prunes what cannot be built, run or read by
   a query output.
5. **Stage 3** prunes dominated and uncostable candidates, solves the
   selection MILP (cost model 1, accuracy by constraint (K)), and builds the
   plan from the selected executions and edges.

No stage caps the candidates it plans, and neither does `asap_planner`.

## Verification

- Unit tests sit beside each module (`cargo nextest run -p
  asap-stage0-memo -p asap-stage1-pass1-per-computation-candidates -p
  asap-stage1-pass2-reuse-candidates -p asap-stage2-physical-optimization -p
  asap-stage3-plan-selection -E 'test(<pattern>)'`).
- `crates/stage1-pass1-per-computation-candidates/src/per_computation_candidates_tests.rs` covers
  Pass 1.
- `crates/integration-tests/tests/planner_layering_example{1,2,3,4}.rs` pin
  the design document's examples through every stage;
  `reuse_rules.rs`, `deployment_capabilities.rs`, `physical_properties_e2e.rs`
  and `stage1_logical_asap_optimization.rs` cover single rules end to end.
- `crates/devtools/tests/asap_planner.rs` checks the stage document and the
  committed viewer examples; `asap_planner_acceptance.rs` checks
  `--run-until` and the refused flags.
