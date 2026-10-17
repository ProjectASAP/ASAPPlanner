# ASAP-Aware Mapping architecture

Audience: developers changing the planner. This document maps the planning
stages of the [design](../design_docs/proposals/planner-layering.md) to the
code. For the public types and their rules, read the
[contracts](asap-aware-mapping-contracts.md); to add behavior, follow
[Extend ASAP-aware mapping](extend-asap-aware-mapping.md).

## Crates

| Crate | Role |
| --- | --- |
| `asap-types` (`crates/types`) | The IR (`ir`: `OperatorNode`, `NonASAPOp`, `ASAPOp`, timing, export), workload types (`workload`), accuracy types (`types`: `AccuracyTarget`, `AccuracyMetric`), summary types (`post_asap`: `SketchAlgorithm`, `FamilyCapability`, `AccuracyGuarantee`) |
| `asap-frontend-sql`, `asap-frontend-promql`, `asap-frontend-metricsql` (with `asap-frontend-common`) | Stage 0, the query-language frontends: lower each query into the logical DAG (`QueryRoot`s of `NonASAPOp` nodes, no summaries, no timing) |
| `asap-aware-mapping` (`crates/asap-aware-mapping`) | Stages 1–3 |
| `asap-planner` (`crates/planner`) | The facade: `e2e_plan` runs Stage 0, then the optimization pass (`Stage1ToStage3Pass` by default) |
| `asap-physical-operators` | The executor's operators and summary kernels over `asap_sketchlib`; it runs a `PhysicalASAPDAG` |
| `asap-devtools` | `stage_pipeline` and the other inspection tools, the design-doc example workloads (`planner_layering_examples`) and the candidate labels |

Dependencies point one way: the frontends and `asap-aware-mapping` depend on
`asap-types`, never on each other; `asap-planner` depends on all of them.

## The stages in `asap-aware-mapping`

```text
QueryRoots (Stage 0)
  │  per_computation_candidates::generate_per_computation_candidates     Stage 1, Pass 1
  ▼
PerComputationCandidates  ──without_unsupported_summaries(deployment)──┐
  │  pass::stage1_to_stage3::stage1_logical_asap_optimization           │ Stage 1, enumeration
  │    compose_logical_candidate per choice, then asap_aware_cse rules  │ + Pass 2
  ▼                                                                    │
Stage1Candidate / LogicalCandidate (each with its Pass2Candidate)  ◄──┘
  │  stage2_physical_asap_optimization::optimize_physical_asap          Stage 2
  ▼
PhysicalCandidate (one per materialization assignment)
  │  stage3_plan_selection::select_plan                                 Stage 3
  ▼
Selection (selected id and plan, costs, accepted, rejected, front)
```

`pass::run_stage1_to_stage3` runs all of it over lowered roots and returns
every stage's candidates (`Stage1ToStage3Run`); `Stage1ToStage3Pass` is the
same pipeline behind the `OptimizationPass` trait that `asap-planner` calls.

| Concern | Where it lives |
| --- | --- |
| Pass 1: which computations a query has (`ComputationTarget`), how each can be computed (`ComputationCandidate`: `Exact`, `ExactAccumulator`, `Summary`), fused computations (`FusedComputation`), pruning (`PruneReason`) and composing one choice into a logical DAG (`compose_logical_candidate`) | `per_computation_candidates.rs` |
| Which summary families serve an intent (`summary_candidates`), the intent's accuracy target and (ε, δ) budget, default sizing | `realization.rs` |
| Pass 2 rules: identical-expression, summary-capability, window-composition (tumbling panes), rollup and exact top-k reuse; each produces `*Candidate` parts and `*Pass2Candidate` whole-workload plans | `asap_aware_cse.rs` |
| Enumeration of Stage 1 candidates (the Cartesian product of Pass 1 choices, each followed by its Pass 2 candidates) | `pass/stage1_to_stage3.rs` |
| What each summary family can merge and answer (`FamilyCapability::of`: `merge_inputs`, `readouts`) | `asap-types`, `post_asap/family_capability.rs` |
| Stage 2: materialization options per summary build (`NotMaterialized`, `MatAtIngestion`, `MatAtQuery`, `MatAtQueryKeptAcrossEvaluations`), ingestion-time eligibility, physical operator implementation | `stage2_physical_asap_optimization.rs` |
| Timing and data-state edge rules applied from a `MaterializationAssignment` | `asap-types`, `ir/timing.rs` and `post_asap/execution_data_state.rs` |
| Stage 3: accuracy check, latency check, deployment-capability checks, resource usage (`ResourceUsage`), cost models 1–3 (`CostModel`), measured operation costs | `stage3_plan_selection.rs`, `stage3_plan_selection/measured_operation_costs.rs` |
| The analytical per-operator resource model Stage 3 measures with | `analytical_cost.rs`, `physical_operator_statistics.rs` |
| The accuracy algebra: per-family guarantees, composition, budget split | `accuracy/` (`AccuracyModel`, `DefaultAccuracyModel`, `estimators/`, `composition.rs`, `budget.rs`) |
| What the executor supports (`DeploymentCapabilities`) | `deployment_capabilities.rs` |
| The inputs every stage reads (`PlanningModels`: accuracy model, evidence, plan selection settings, deployment capabilities) and the `OptimizationPass` trait | `pass/mod.rs` |

Not yet wired into the stages: `maintained_population.rs` (maintained
populations for instant top-k, quantile and sum), `pane_sharing.rs`,
`storage_io.rs`, `physical_handoff_cost.rs` and `query_physical_lowering.rs`.

## Algorithm in one pass

1. **Pass 1** walks each root, finds every aggregate computation, and gives it
   one exact option plus the summary families of `summary_candidates` that the
   accuracy target permits and the deployment supports. A whole-expression
   top-k or a nested sum also gets a fused computation that absorbs the
   target beneath it.
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
   `DataWorkload`), every query's latency against `max_latency_ms`, raw-data
   scans against `raw_data_retained`, and, under cost model 3, the cluster's
   limits. It measures the valid ones and selects one under the cost model;
   ties keep enumeration order.

No stage caps the candidates it plans; `stage_pipeline` caps only what it
displays and refuses workloads over `--max-combinations`.

## Verification

- Unit tests sit beside each module (`cargo nextest run -p asap-aware-mapping
  -E 'test(<pattern>)'`).
- `crates/asap-aware-mapping/tests/per_computation_candidates.rs` covers
  Pass 1.
- `crates/integration-tests/tests/planner_layering_example{1,2,3,4}.rs` pin
  the design document's examples through every stage;
  `reuse_rules.rs`, `deployment_capabilities.rs`, `materialization_e2e.rs`
  and `stage1_logical_asap_optimization.rs` cover single rules end to end.
- `crates/devtools/tests/stage_pipeline.rs` checks the stage document and the
  committed viewer example.
