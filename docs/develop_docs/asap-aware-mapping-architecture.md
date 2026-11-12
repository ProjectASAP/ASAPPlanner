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
| `asap-types` (`crates/types`) | Shared vocabulary, by scope: the operator IR by IR section, `ir::operator` (`OperatorNode`, `NonASAPOp`, `ASAPOp`, operator arguments, `AggIntent`), `ir::scalar` (`ScalarExpr`, `ColumnRef`), `ir::schema` (`Schema`, `FieldDataType`), `ir::properties` (`execution_data_state`: execution timing and what an edge carries; `selection`: the selection algebra aggregate coverage is stated in) and `ir::flat`; summary types (`summary_type`: `SketchAlgorithm`, `SketchParams`, `SummaryTypeCapability`); accuracy targets and guarantees (`accuracy`: `AccuracyTarget`, `AccuracyMetric`, `AccuracyGuarantee`); the planner inputs (`workload`); the stage-boundary types (`stages::logical`: the memo, its groups with their aggregate coverage, candidates and logical edges; `stages::physical`: Stage 2's physical candidates with their `PhysicalProperties` (timing and `Lifetime`) and edges; `stages::pruning`); and Stage 3's plan (`plan`: `plan::physical_asap_dag`, `plan::materialized_view`) |
| `asap-frontend-sql`, `asap-frontend-promql`, `asap-frontend-metricsql` (with `asap-frontend-common`) | Stage 0, the query-language frontends: lower each query into the logical DAG (`QueryRoot`s of `NonASAPOp` nodes, no summaries, no timing) |
| `asap-stage0-memo` (`crates/stage0-memo`) | Stage 0 hash-consing: the memo groups of the workload's query roots (`memo_of_queries`) |
| `asap-stage1-pass1-per-computation-candidates` (`crates/stage1-pass1-per-computation-candidates`) | Stage 1, Pass 1: the initial candidates of each memo group |
| `asap-stage1-pass2-reuse-candidates` (`crates/stage1-pass2-reuse-candidates`) | Stage 1, Pass 2: the reuse rules over memo groups and the logical edges |
| `asap-accuracy` (`crates/accuracy`) | The accuracy model the stages share: the analytical rules (per-algorithm estimators, each operator's error contract, the target comparison `within_limit`), the deployment's measured accuracy (`AccuracyEvidence`) and α^log (`ErrorMetricsOfMemo::edge_meets_logical_requirements`); depends only on `asap-types` |
| `asap-stage2-physical-optimization` (`crates/stage2-physical-optimization`) | Stage 2: each logical candidate's timing and lifetime, the physical edges and the pruned alternatives; depends only on `asap-types` |
| `asap-cost` (`crates/cost`) | The cost model the stages share: resource-usage estimation (the analytical per-operator model, measured operation costs, state byte sizes) and the cost models (`ResourceUsage`, `CostModel`, `CostInputs`); depends only on `asap-types` |
| `asap-stage3-plan-selection` (`crates/stage3-plan-selection`) | Stage 3: `select_plan` over Stage 2's output; accuracy and latency checks, pricing candidates with `asap-cost` |
| `asap-planner` (`crates/planner`) | The facade: `e2e_plan` runs Stage 0, then the optimization pass (`Stage1ToStage3Pass` by default), which orchestrates Stages 1–3 (`run_planning`); the planning models (`PlanningModels`) and the `OptimizationPass` trait |
| `asap-executor` (`crates/executor`) | Stage 4, the reference executor: operators and summary kernels over `asap_sketchlib`; it runs a `PhysicalASAPDAG` |
| `asap-devtools` | `asap_planner` and the other inspection tools, the design-doc example workloads (`planner_layering_examples`) and the candidate labels |

```text
asap-types ◄── frontends (Stage 0)
asap-types ◄── asap-cost (shared cost model) ◄── asap-stage3-plan-selection
asap-types ◄── asap-accuracy (shared accuracy model) ◄── asap-stage1-pass2-reuse-candidates, asap-stage3-plan-selection
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
Memo (each group's initial candidates)
  │  asap_stage1_pass2_reuse_candidates::generate_logical_asap_memo_candidates
  │    the reuse rules (crates/stage1-pass2-reuse-candidates/src/rules/)
  │    over the candidate memo                                    Stage 1, Pass 2
  ▼
LogicalASAPMemoCandidates
  │  asap_stage2_physical_optimization::generate_physical_asap_memo_candidates  Stage 2
  ▼
PhysicalASAPMemoCandidates (operator nodes per timing/materialization + pruned_candidates)
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
| Pass 1: which computations a query has (`ComputationTarget`), how each can be computed (`HowToComputeAggregateMeasure`: `Exact`, `ExactAccumulator`, `Summary`), fused computations (`FusedComputation`); the candidates that compute them | `asap-stage1-pass1-per-computation-candidates`, `computation_targets.rs`, `candidates/` |
| Which summary families serve an intent (`summary_candidates`) | `asap-stage1-pass1-per-computation-candidates`, `summary_realization.rs` |
| Pass 2 steps: rollup from the finest common group (shared producers across group keys), top-k reuse, readout requirements, dead-candidate elimination, chain groups (tumbling windows) and the logical edges | `asap-stage1-pass2-reuse-candidates` (`crates/stage1-pass2-reuse-candidates/src/`): `lib.rs` (the steps in order), `rules/` (`candidates.rs`, `rollup/`, `topk.rs`, `chains.rs`), `readout_requirements.rs`, `dead_candidates.rs`, `edges.rs` and `evaluation_grids.rs`; the groups each query may read only exactly come from `asap_accuracy::ErrorMetricsOfMemo` |
| Enumeration of the plans a memo stands for (reference pricer, DAG viewer) | none today (the standalone enumeration was deleted) |
| What each summary type can merge and answer (`SummaryTypeCapability::of`: `merge_inputs`, `readouts`) | `asap-types`, `summary_type/capability.rs` |
| Stage 2: the timings and lifetimes offered per logical candidate and their prune rules, the per-group offers (ingestion time, across evaluations), the physical edges, and the alternatives no plan can use | `asap-stage2-physical-optimization`: `candidate.rs`, `group.rs`, `edge.rs`, `pruning.rs`; the admissible-edge indexes in `asap-types`, `stages/physical/memo_candidates/mod.rs` |
| Execution timing and data-state edge rules; each physical candidate's `PhysicalProperties` and `Lifetime`, and the physical edge check (`producer_satisfies_physical_requirements`) | `asap-types`, `ir/properties/execution_data_state.rs`; `stages/physical/properties/mod.rs` (`PhysicalProperties`, `Lifetime`) and `stages/physical/candidate.rs` (`PhysicalCandidate`, the edge check) |
| Stage 3: accuracy check, latency check, resource usage (`ResourceUsage`), cost models 1–3 (`CostModel`), measured operation costs | `asap-stage3-plan-selection`, `lib.rs`; `asap-cost`, `model.rs` (`CostModel`, `CostInputs`), `resources/mod.rs` (`ResourceUsage`) and `measured.rs` |
| The analytical per-operator resource model Stage 3 measures with | `asap-cost`, `node/analytical.rs` and `node/operator_statistics.rs` |
| The accuracy rules: per-family guarantees, each operator's error contract, the metric rule, each readout's required accuracy, α^log of a logical edge | `asap-accuracy` (`AccuracyEvidence`, `theoretical_guarantee`, `guarantee/`, `evidence.rs`, `error_contract/`, `metric_rule/`, `readout_caps/`, `logical_edge/`) |
| The statistic a summary is read for an aggregation intent (`SketchStatistic::for_intent`) | `asap-types`, `summary_type/statistic.rs` |
| What the executor supports (`DeploymentCapabilities`) | `asap-types`, `deployment_capabilities.rs`; Pass 1 receives only `supported_summaries` and Stage 2 only `query_time_results_retained` and `raw_data_retained`; Stage 3 reads none of it |
| The inputs every stage reads (`PlanningModels`: accuracy evidence, cost inputs, deployment capabilities) and the `OptimizationPass` trait | `asap-planner`, `models.rs` (`PlanningModels`) and `pass.rs` (the trait); the facade gives Pass 2 the evidence and Stage 3 only `CostInputs` |

The current production search organizes these computations in memo result/state
groups and jointly selects physical candidates and connections; it does not
traverse the whole-workload Cartesian product described by the historical rule
APIs below. See [joint planning](../design_docs/proposals/joint-planning-search.md)
for the current workflow. The unused standalone tumbling-window-sharing selector has been
removed; tumbling window sharing is selected by the same joint MILP as other computation.
The unwired maintained-population rule has been removed as well; no Stage 1 pass offers a population candidate.

## Algorithm in one pass

1. **Pass 1** walks each root, finds every aggregate computation, and gives it
   one exact option plus the summary families of `summary_candidates` that the
   accuracy target permits and the deployment supports. A whole-expression
   top-k or a nested sum also gets a fused computation that absorbs the
   targets beneath it.
2. **Enumeration** takes one choice per target and composes the logical DAG.
   For each composed DAG, the Pass 2 rules find what can be shared, split into
   tumbling windows or reused, and each subset of their candidates becomes another
   Stage 1 candidate.
3. **Stage 2** expands each logical candidate into one physical candidate
   per offered timing and lifetime, each logical edge into the physical
   edges between their alternatives, and records the alternatives it
   prunes.
4. **Stage 3** checks each physical candidate: every query's root guarantee
   against its target (binding the statistics Stage 1 left symbolic from the
   `DataWorkload`), every query's latency against `max_latency_ms` and, for a
   repeating query, against its repeat interval (with or without a bound), raw-data
   scans against `raw_data_retained`, and, under cost model 3, the cluster's
   limits. It measures the valid ones and selects one under the cost model;
   ties keep enumeration order.

No stage caps the candidates it plans, and neither does `asap_planner`.

## Verification

- Unit tests sit beside each module (`cargo nextest run -p
  asap-stage0-memo -p asap-stage1-pass1-per-computation-candidates -p
  asap-stage1-pass2-reuse-candidates -p asap-stage2-physical-optimization -p
  asap-stage3-plan-selection -E 'test(<pattern>)'`).
- `crates/stage1-pass1-per-computation-candidates/src/tests.rs` covers
  Pass 1.
- `crates/integration-tests/tests/planner_layering_example{1,2,3,4}.rs` pin
  the design document's examples through every stage;
  `reuse_rules.rs`, `deployment_capabilities.rs`, `physical_properties_e2e.rs`
  and `pass1_candidates.rs` cover single rules end to end.
- `crates/devtools/tests/asap_planner.rs` checks the stage document and the
  committed viewer examples; `asap_planner_acceptance.rs` checks
  `--run-until` and the refused flags.
