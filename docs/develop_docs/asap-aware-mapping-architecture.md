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
| `asap-types` (`crates/types`) | Shared vocabulary, by IR section: `ir::operator` (`OperatorNode`, `NonASAPOp`, `ASAPOp`, operator parameters, `AggIntent`), `ir::scalar` (`ScalarExpr`, `ColumnRef`), `ir::schema` (`Schema`, `FieldDataType`, summary-state types such as `SketchAlgorithm` and `FamilyCapability`), `ir::properties` (`AccuracyTarget`, `AccuracyMetric`, `AccuracyGuarantee`, execution timing, summary coverage), plus `ir::flat` and `ir::physical_export`; the planner inputs (`workload`); Stage 2 decision data (`physical`: `Materialization`, panes) |
| `asap-frontend-sql`, `asap-frontend-promql`, `asap-frontend-metricsql` (with `asap-frontend-common`) | Stage 0, the query-language frontends: lower each query into the logical DAG (`QueryRoot`s of `NonASAPOp` nodes, no summaries, no timing) |
| `asap-logical-asap-optimization` (`crates/logical-asap-optimization`) | Stage 1: Pass 1 and Pass 2 (the memo rules) |
| `asap-accuracy` (`crates/accuracy`) | The accuracy model the stages share: the analytical rules (per-family estimators, composition, budget split, target check) and the deployment's `AccuracyModel` and `AccuracyEvidenceProvider`; depends only on `asap-types` |
| `asap-physical-asap-optimization` (`crates/physical-asap-optimization`) | Stage 2: materialization and physical operator implementation |
| `asap-cost` (`crates/cost`) | The cost model the stages share: resource-usage estimation (the analytical per-operator model, measured operation costs, state byte sizes) and the cost models (`ResourceUsage`, `CostModel`, `CostInputs`); depends only on `asap-types` |
| `asap-plan-selection` (`crates/plan-selection`) | Stage 3: `select_plan` over Stage 2's output; accuracy and latency checks, pricing candidates with `asap-cost` |
| `asap-planner` (`crates/planner`) | The facade: `e2e_plan` runs Stage 0, then the optimization pass (`Stage1ToStage3Pass` by default), which orchestrates Stages 1–3 (`run_planning`); the planning models (`PlanningModels`) and the `OptimizationPass` trait |
| `asap-executor` (`crates/executor`) | Stage 4, the reference executor: operators and summary kernels over `asap_sketchlib`; it runs a `PhysicalASAPDAG` |
| `asap-devtools` | `stage_pipeline` and the other inspection tools, the design-doc example workloads (`planner_layering_examples`) and the candidate labels |

```text
asap-types ◄── frontends (Stage 0)
asap-types ◄── asap-cost (shared cost model) ◄── asap-physical-asap-optimization, asap-plan-selection
asap-types ◄── asap-accuracy (shared accuracy model) ◄── asap-logical-asap-optimization, asap-physical-asap-optimization, asap-plan-selection
asap-types ◄── asap-logical-asap-optimization (Stage 1) ◄── asap-planner
asap-types ◄── asap-physical-asap-optimization (Stage 2) ◄── asap-planner
asap-plan-selection (Stage 3) ◄── asap-planner (also depends on the frontends)
asap-types ◄── asap-executor (Stage 4; no stage crate depends on it)
```

## The stages

```text
QueryRoots (Stage 0)
  │  generate_dag_of_logical_memo_groups → DagOfLogicalMemoGroups, then
  │  generate_per_computation_logical_memo_candidates
  ▼                                                                     Stage 1, Pass 1
PerComputationLogicalMemoCandidates (MemoCandidates + pruned_candidates)
  │  generate_logical_asap_memo_candidates
  │    the memo rules (pass2/memo/rules/) over the candidate memo       Stage 1, Pass 2
  ▼
LogicalASAPMemoCandidates
  │  asap_physical_asap_optimization::generate_physical_asap_memo_candidates  Stage 2
  ▼
PhysicalASAPMemoCandidates (operator nodes per timing/materialization + pruned_candidates)
  │  asap_plan_selection::select_plan (with Stage 2's AssignmentRealizer) Stage 3
  ▼
Selection (selected_dag, cost, accepted_accuracy_per_query, search_report)
```

`asap_planner::run_planning` runs all of it over lowered roots: Pass 1 once,
then Pass 2, Stage 2 and Stage 3 per pane-width round, keeping the best
round's selection and building that plan with `AssignmentRealizer::realize`.
It returns every stage's output (`PlanningRun`); `Stage1ToStage3Pass` is the
same pipeline behind the `OptimizationPass` trait.

| Concern | Where it lives |
| --- | --- |
| Pass 1: which computations a query has (`ComputationTarget`), how each can be computed (`HowToComputeAggregateMeasure`: `Exact`, `ExactAccumulator`, `Summary`), fused computations (`FusedComputation`), pruning (`PruneReason`) and composing one choice into a logical DAG (`compose_logical_candidate`) | `asap-logical-asap-optimization`, `pass1/per_computation_candidates.rs` |
| Which summary families serve an intent (`summary_candidates`) | `asap-logical-asap-optimization`, `pass1/realization.rs` |
| Pass 2 memo rules: shared producers (identical expressions by memo group identity, one build at different sizes), scan-label widening, window composition (tumbling panes), rollup, exact top-k reuse and finer-aggregate reuse | `asap-logical-asap-optimization`, `pass2/memo/rules/` and `pass2/memo/window_candidates.rs`, over the helpers in `pass2/asap_aware_cse.rs` |
| Enumeration of the plans a memo stands for (reference pricer, DAG viewer) | `asap-logical-asap-optimization`, `pass2/memo/enumeration.rs` |
| What each summary family can merge and answer (`FamilyCapability::of`: `merge_inputs`, `readouts`) | `asap-types`, `ir/schema/family_capability.rs` |
| Stage 2: materialization options per summary build (`NotMaterialized`, `MatAtIngestion`, `MatAtQuery`, `MatAtQueryKeptAcrossEvaluations`), ingestion-time eligibility, physical operator implementation | `asap-physical-asap-optimization` |
| Timing and data-state edge rules applied from a `MaterializationAssignment` | `asap-types`, `ir/properties/timing.rs` and `ir/properties/execution_data_state.rs`; `Materialization` in `physical/materialization.rs` |
| Stage 3: accuracy check, latency check, resource usage (`ResourceUsage`), cost models 1–3 (`CostModel`), measured operation costs | `asap-plan-selection`, `lib.rs`; `asap-cost`, `lib.rs` and `measured.rs` |
| The analytical per-operator resource model Stage 3 measures with | `asap-cost`, `analytical.rs` and `operator_statistics.rs` |
| The accuracy algebra: per-family guarantees, composition, budget split; guarantee propagation through a DAG; sizing a summary for its target, (ε, δ) budget and default sizes | `asap-accuracy` (`AccuracyModel`, `BuiltinAccuracyModel`, `estimators/`, `composition.rs`, `budget.rs`, `propagation.rs`, `sizing.rs`) |
| What the executor supports (`DeploymentCapabilities`) | `asap-logical-asap-optimization`, `deployment_capabilities.rs`; Pass 1 receives only `supported_summaries` and Stage 2 only `query_time_results_retained` and `raw_data_retained` (`bool` arguments, so Stage 2 does not depend on the Stage 1 crate); Stage 3 reads none of it |
| The inputs every stage reads (`PlanningModels`: accuracy model, evidence, cost inputs, deployment capabilities) and the `OptimizationPass` trait | `asap-planner`, `pass.rs`; the facade gives Stage 3 only `AccuracyCheckInputs` and `CostInputs` |

The current production search organizes these computations in memo result/state
groups and jointly selects physical candidates and connections; it does not
traverse the whole-workload Cartesian product described by the historical rule
APIs below. See [joint planning](../design_docs/proposals/joint-planning-search.md)
for the current workflow. The unused standalone pane-sharing selector has been
removed; pane sharing is selected by the same joint MILP as other computation.
The unwired maintained-population rule has been removed as well; no Stage 1 pass offers a population candidate.

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
