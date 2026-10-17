# ASAP-Aware Mapping contracts

Audience: developers changing the planner. This reference states the rules
the stage code relies on: what each stage's input and output may contain,
and which component owns each decision. Read the
[architecture](asap-aware-mapping-architecture.md) first; use the
[extension guide](extend-asap-aware-mapping.md) when changing one. Field-level
details are in the rustdoc (`cargo doc -p asap-aware-mapping -p asap-types
--no-deps`).

## 1. IR at each stage

| After | The DAG holds | Timing |
| --- | --- | --- |
| Stage 0 (frontends) | `NonASAPOp` nodes only; aggregates carry `AggIntent` measures with each query's `AccuracyTarget` as written | none |
| Stage 1 (`compose_logical_candidate`, Pass 2 rules) | `ASAPOp` nodes where a candidate chose an exact accumulator or a summary (`SummaryAgg`, `PaneChain`, `SummaryMerge`, `SummaryEstimate`, `FinalizeExactAccumulator`); everything else stays `NonASAPOp` | none: Stage 1 never decides when anything runs |
| Stage 2 (`optimize_physical_asap`) | the same operators, with a `Materialization` on each summary build | every node timed from the candidate's `MaterializationAssignment` |

- Structurally equal sub-DAGs are interned before Stage 1
  (`asap_types::ir::cse`), so Stage 1 compares nodes by structure and Pass 2
  matches readers structurally, never by pointer.
- Timing is applied with `apply_materialization_timings`, which validates
  every data-state edge. A violation is an `ExecutionDataStateError`:
  for example `QueryTimeValueUnderIngestionSummary` (a query-time value
  beneath an ingestion-time build), `ConflictingTiming` (one shared node
  needs two timings; `split_shared_by_timing` copies it for one side first)
  or `IllegalChildDataState`. These are internal errors in a candidate the
  stages built, not bad user input.
- A summary build's `SummaryCoverage` (which rows and time range its state
  represents) is derived, never declared; a merge's inputs must be disjoint
  as its family's `MergeInputRule` requires, or coverage fails
  (`InputsOverlap`).

## 2. Stage 1 candidates

- **Targets.** `ComputationTarget` is one measure of one `Aggregate`. Its
  `choices` list the exact option first (`ExactAccumulator` when the intent
  has a mergeable exact state, else `Exact`), then each `Summary(family)` from
  `realization::summary_candidates`. Candidates are unranked: Pass 1 never
  compares cost.
- **Pruning.** A candidate that cannot meet the target is moved to the
  target's pruned list with a `PruneReason` (`RequiresExact`,
  `NullableAvgInput`, `UnsupportedByDeployment`); it is never silently
  dropped. Pass 1 prunes only what it can prove; accuracy that depends on
  data facts is left to Stage 3.
- **Fused computations.** `FusedComputation { absorbs, candidates, pruned }`
  replaces the target directly beneath (the whole-expression top-k, the
  nested sum). A whole-workload choice keeps one digit per target; a fused
  pick with a non-first digit on the absorbed target is skipped, so each
  plan is enumerated once.
- **Pass 2.** Each rule adds candidates; none removes a Pass 1 candidate, adds
  a summary family, or adds a readout. So a deployment that does not support a
  family never sees it again after Pass 1. A Pass 2 candidate records what it
  changed in `Pass2Candidate` (shared builds, panes, rollups, top-k reuses,
  identical-node sharing) and keeps its Pass 1 candidate's `choice`.
- **Sizing.** A summary shared by several readers is sized with each
  parameter's largest value among them. A summary is sized for ε in Stage 1;
  a δ that depends on data facts (a distinct-count bound, series counts) is
  left symbolic in its guarantee for Stage 3 to bind.

## 3. Accuracy

- `AccuracyModel` is the extension point: `local_guarantee` (a family's
  guarantee over exact input), `propagate` (composition through an operator)
  and `satisfies` (a guarantee against a target). It fails closed: no rule, or
  a missing input guarantee, means no guarantee, and the candidate is
  rejected rather than assumed accurate.
- A target with no `accuracy_metric` accepts a guarantee in any metric that
  meets its ε, δ and scope; Stage 3 reports the metric it accepted
  (`AcceptedAccuracy`).
- An approximate input split across several inputs gets its share through
  `accuracy::budget::per_input_target`.
- Facts come only from the declared `DataWorkload`; an undeclared fact is
  unknown, never zero.

## 4. Stage 2

- Only summary builds (`SummaryAgg`, `PaneChain`) get materialization
  options: `NotMaterialized` always; `MatAtIngestion(Memory)` when the build
  is eligible to run as data arrives; `MatAtQuery(Memory)` when two or more
  queries read it (reused within one evaluation);
  `MatAtQueryKeptAcrossEvaluations(Memory)` for a tumbling-pane chain that
  builds one new pane per evaluation, when the deployment declares
  `query_time_results_retained`.
- One `PhysicalCandidate` per assignment, the all-`NotMaterialized` one first
  (`P<n>`, then `P<n>-m<k>`). An assignment that puts a build at ingestion
  time over one that is not is not generated. A logical candidate Stage 2
  cannot implement becomes a `PrunedCandidate` with its reason.

## 5. Stage 3

- `select_plan` validates its inputs first: `SelectionError::InvalidSettings`,
  `EntryCount` and `InvalidDeploymentCapabilities` are bad input and fail the
  whole selection.
- Each candidate is then either invalid or valid, recorded as a `Rejection`
  with its reason (`valid: false` failed a check: accuracy, latency, raw data
  not retained, or a cluster limit under cost model 3; `valid: true` lost
  under the cost model). A candidate that cannot be resized for its failure
  probability is rejected alone.
- Usage (`ResourceUsage`) is measured without prices; the `CostModel` in
  `PlanSelectionSettings` turns it into a choice. Ties keep enumeration order
  under every cost model.
- `Selection.plan` is the selected candidate's roots with the weight
  requirements Stage 3 confirmed from the data facts.

## 6. Ownership

| Decision | Owner |
| --- | --- |
| What a query means | the frontend (Stage 0) |
| Which computations exist and how each may be computed | Pass 1 |
| What is shared, split into panes or reused | Pass 2 |
| When each build runs and whether it is kept | Stage 2 |
| Which plan is valid and which is chosen | Stage 3 |
| What the executor can build, retain and how it is priced | the deployment, through `PlanningModels` (`DeploymentCapabilities`, `PlanSelectionSettings`, `AccuracyModel`) |
| Running the plan | the executor (`asap-physical-operators`), outside the planner |
