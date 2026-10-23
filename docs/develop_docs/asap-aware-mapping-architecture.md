# ASAP-Aware Mapping architecture

Audience: developers changing the planner. This document maps the planning
stages of the [design](../design_docs/proposals/planner-layering.md) to the
code. For the public types and their rules, read the
[contracts](asap-aware-mapping-contracts.md); to add behavior, follow
[Extend ASAP-aware mapping](extend-asap-aware-mapping.md).

## Crates

One crate per planning stage, so Cargo enforces the one-way stage flow: a
stage crate depends only on `asap-types` and the stages before it, and none
depends on the executor.

| Crate | Role |
| --- | --- |
| `asap-types` (`crates/types`) | Shared vocabulary, by IR section: `ir::operator` (`OperatorNode`, `NonASAPOp`, `ASAPOp`, operator parameters, `AggIntent`), `ir::scalar` (`ScalarExpr`, `ColumnRef`), `ir::schema` (`Schema`, `FieldDataType`, summary-state types such as `SketchAlgorithm` and `FamilyCapability`), `ir::properties` (`AccuracyTarget`, `AccuracyMetric`, `AccuracyGuarantee`, execution timing, summary coverage), plus `ir::flat` and `ir::physical_export`; the planner inputs (`workload`); Stage 2 decision data (`physical`: `Materialization`, panes) |
| `asap-frontend-sql`, `asap-frontend-promql`, `asap-frontend-metricsql` (with `asap-frontend-common`) | Stage 0, the query-language frontends: lower each query into the logical DAG (`QueryRoot`s of `NonASAPOp` nodes, no summaries, no timing) |
| `asap-logical-asap-optimization` (`crates/logical-asap-optimization`) | Stage 1: Pass 1, Pass 2, the whole-workload Stage 1 candidates and the analytical accuracy model |
| `asap-physical-asap-optimization` (`crates/physical-asap-optimization`) | Stage 2: materialization and physical operator implementation |
| `asap-plan-selection` (`crates/plan-selection`) | Stage 3: accuracy and latency checks, resource usage and cost models; the planning models (`PlanningModels`) and the Stage 1 to Stage 3 driver (`pass`) |
| `asap-planner` (`crates/planner`) | The facade: `e2e_plan` runs Stage 0, then the optimization pass (`Stage1ToStage3Pass` by default) |
| `asap-executor` (`crates/executor`) | Stage 4, the reference executor: operators and summary kernels over `asap_sketchlib`; it runs a `PhysicalASAPDAG` |
| `asap-devtools` | `stage_pipeline` and the other inspection tools, the design-doc example workloads (`planner_layering_examples`) and the candidate labels |

```text
asap-types ◄── frontends (Stage 0)
asap-types ◄── asap-logical-asap-optimization (Stage 1)
                 ◄── asap-physical-asap-optimization (Stage 2)
                       ◄── asap-plan-selection (Stage 3, also depends on Stage 1)
                             ◄── asap-planner (also depends on the frontends)
asap-types ◄── asap-executor (Stage 4; no stage crate depends on it)
```

## The stages

```text
QueryRoots (Stage 0)
  │  per_computation_candidates::generate_per_computation_candidates     Stage 1, Pass 1
  ▼
PerComputationCandidates  ──without_unsupported_summaries(deployment)──┐
  │  workload_candidates::stage1_logical_asap_optimization              │ Stage 1, enumeration
  │    compose_logical_candidate per choice, then asap_aware_cse rules  │ + Pass 2
  ▼                                                                    │
Stage1Candidate / LogicalCandidate (each with its Pass2Candidate)  ◄──┘
  │  asap_physical_asap_optimization::optimize_physical_asap            Stage 2
  ▼
PhysicalCandidate (one per materialization assignment)
  │  asap_plan_selection::select_plan                                   Stage 3
  ▼
Selection (selected id and plan, costs, accepted, rejected, front)
```

`asap_plan_selection::pass::run_stage1_to_stage3` runs all of it over lowered roots and returns
every stage's candidates (`Stage1ToStage3Run`); `Stage1ToStage3Pass` is the
same pipeline behind the `OptimizationPass` trait that `asap-planner` calls.

| Concern | Where it lives |
| --- | --- |
| Pass 1: which computations a query has (`ComputationTarget`), how each can be computed (`ComputationCandidate`: `Exact`, `ExactAccumulator`, `Summary`), fused computations (`FusedComputation`), pruning (`PruneReason`) and composing one choice into a logical DAG (`compose_logical_candidate`) | `asap-logical-asap-optimization`, `per_computation_candidates.rs` |
| Which summary families serve an intent (`summary_candidates`), the intent's accuracy target and (ε, δ) budget, default sizing | `asap-logical-asap-optimization`, `realization.rs` |
| Pass 2 rules: identical-expression, summary-capability, window-composition (tumbling panes), rollup and exact top-k reuse; each produces `*Candidate` parts and `*Pass2Candidate` whole-workload plans | `asap-logical-asap-optimization`, `asap_aware_cse.rs` |
| Enumeration of Stage 1 candidates (the Cartesian product of Pass 1 choices, each followed by its Pass 2 candidates) | `asap-logical-asap-optimization`, `workload_candidates.rs` |
| What each summary family can merge and answer (`FamilyCapability::of`: `merge_inputs`, `readouts`) | `asap-types`, `ir/schema/family_capability.rs` |
| Stage 2: materialization options per summary build (`NotMaterialized`, `MatAtIngestion`, `MatAtQuery`, `MatAtQueryKeptAcrossEvaluations`), ingestion-time eligibility, physical operator implementation | `asap-physical-asap-optimization` |
| Timing and data-state edge rules applied from a `MaterializationAssignment` | `asap-types`, `ir/properties/timing.rs` and `ir/properties/execution_data_state.rs`; `Materialization` in `physical/materialization.rs` |
| Stage 3: accuracy check, latency check, deployment-capability checks, resource usage (`ResourceUsage`), cost models 1–3 (`CostModel`), measured operation costs | `asap-plan-selection`, `lib.rs` and `cost/measured.rs` |
| The analytical per-operator resource model Stage 3 measures with | `asap-plan-selection`, `cost/analytical.rs` and `cost/operator_statistics.rs` |
| The accuracy algebra: per-family guarantees, composition, budget split | `asap-logical-asap-optimization`, `accuracy/` (`AccuracyModel`, `DefaultAccuracyModel`, `estimators/`, `composition.rs`, `budget.rs`) |
| What the executor supports (`DeploymentCapabilities`) | `asap-logical-asap-optimization`, `deployment_capabilities.rs` |
| The inputs every stage reads (`PlanningModels`: accuracy model, evidence, plan selection settings, deployment capabilities) and the `OptimizationPass` trait | `asap-plan-selection`, `pass/mod.rs` |

Not yet wired into the stages: `maintained_population.rs` (maintained
populations for instant top-k, quantile and sum) in Stage 1, and
`pane_sharing.rs` in plan selection.

## Algorithm in one pass

1. **Pass 1** walks each root, finds every aggregate computation, and gives it
   one exact option plus the summary families of `summary_candidates` that the
   accuracy target permits and the deployment supports. A whole-expression
   top-k or a nested sum also gets a fused computation that absorbs the
   targets beneath it.
2. **Enumeration** takes one choice per target and composes the logical DAG.
   For each composed DAG, the Pass 2 rules find what can be shared, split into
   panes or reused, and each subset of their candidates becomes another
   Stage 1 candidate.
3. **Stage 2** gives each summary build its materialization options and emits
   one physical candidate per assignment (the all-`NotMaterialized` one
   first), skipping assignments that put a build at ingestion time over one
   that is not.
4. **Stage 3** checks each physical candidate: every query's root guarantee
   against its target (binding the statistics Stage 1 left symbolic from the
   `DataWorkload`), every query's latency against `max_latency_ms` and, for a
   repeating query, against its repeat interval (with or without a bound), raw-data
   scans against `raw_data_retained`, and, under cost model 3, the cluster's
   limits. It measures the valid ones and selects one under the cost model;
   ties keep enumeration order.

No stage caps the candidates it plans; `stage_pipeline` caps only what it
displays and refuses workloads over `--max-combinations`.

## Verification

- Unit tests sit beside each module (`cargo nextest run -p
  asap-logical-asap-optimization -p asap-physical-asap-optimization -p
  asap-plan-selection -E 'test(<pattern>)'`).
- `crates/logical-asap-optimization/tests/per_computation_candidates.rs` covers
  Pass 1.
- `crates/integration-tests/tests/planner_layering_example{1,2,3,4}.rs` pin
  the design document's examples through every stage;
  `reuse_rules.rs`, `deployment_capabilities.rs`, `materialization_e2e.rs`
  and `stage1_logical_asap_optimization.rs` cover single rules end to end.
- `crates/devtools/tests/stage_pipeline.rs` checks the stage document and the
  committed viewer example.
