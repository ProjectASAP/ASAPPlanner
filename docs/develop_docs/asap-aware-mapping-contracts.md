# ASAP-Aware Mapping contracts

Audience: developers changing the planner. This reference states the rules
the stage code relies on: what each stage's input and output may contain,
and which component owns each decision. Read the
[architecture](asap-aware-mapping-architecture.md) first; use the
[extension guide](extend-asap-aware-mapping.md) when changing one. Field-level
details are in the rustdoc (`cargo doc -p asap-logical-asap-optimization -p
asap-physical-asap-optimization -p asap-plan-selection -p asap-types
--no-deps`).

## 1. IR at each stage

| After | The DAG holds | Timing |
| --- | --- | --- |
| Stage 0 (frontends) | `NonASAPOp` nodes only; aggregates carry `AggIntent` measures with each query's `AccuracyTarget` as written | none |
| Stage 1 (`compose_logical_candidate`, Pass 2 rules) | `ASAPOp` nodes where a candidate chose an exact accumulator or a summary (`SummaryAgg`, `PaneChain`, `SummaryMerge`, `SummaryEstimate`, `FinalizeExactAccumulator`); everything else stays `NonASAPOp` | none: Stage 1 never decides when anything runs |
| Stage 2 (`optimize_physical_asap`) | the same operators, with a `Materialization` on each summary build | every node timed from the candidate's `MaterializationAssignment` |

- Structurally equal sub-DAGs are interned before Stage 1
  (`asap_logical_asap_optimization::pass2::cse`), so Stage 1 compares nodes by structure and Pass 2
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
  `ways_to_compute` list the exact option first (`ExactAccumulator` when the intent
  has a mergeable exact state, else `Exact`), then each `Summary(family)` from
  `realization::summary_candidates`. Candidates are unranked: Pass 1 never
  compares cost.
- **Pruning.** A candidate that cannot meet the target is moved to the
  inventory's `pruned_ways_to_compute` (with its target) with a `PruneReason` (`RequiresExact`,
  `NullableAvgInput`, `UnsupportedByDeployment`,
  `QuantileOutsideUnitInterval`); it is never silently
  dropped. Pass 1 prunes only what it can prove; accuracy that depends on
  data facts is left to Stage 3.
- **Fused computations.** `FusedComputation { absorbed_targets, ways_to_compute }`
  replaces the targets beneath (the whole-expression top-k, the nested sum).
  It absorbs the target directly beneath and, for a heap over the raw data samples
  beneath a grouped sum of per-series sums or counts, the per-series target
  too. A whole-workload choice keeps one digit per target; a fused pick with
  a non-first digit on an absorbed target is skipped, so each plan is
  enumerated once.
- **Pass 2.** Each rule adds candidates; none removes a Pass 1 candidate, adds
  a summary family, or adds a readout. So a deployment that does not support a
  family never sees it again after Pass 1.
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
- On the memo (`generate_physical_asap_memo_candidates`), every physical
  candidate Stage 2 prunes is recorded in
  `PhysicalASAPMemoCandidates::pruned_candidates` as
  `PrunedCandidate<PhysicalCandidateKey>` with a `PruneReason`: an exact
  per-entity `Aggregate` the executor cannot run
  (`PerEntityAggregateNotExecutable`) and, without raw data retained, a
  query-time scan of raw data (`RawDataNotRetained`) are not generated; a
  candidate no buildable input, query output or reader count can use is
  removed (`InputHasNoBuildableProducer`, `ReachesNoQueryOutput`,
  `TooFewQueryReaders`, `PreviousScopeCopyPruned`).

## 5. Stage 3

- `select_plan` validates its inputs first: `ErrorSelectingPlan::InvalidSettings`
  and `EntryCount` are bad input and fail the whole selection. The facade
  validates the deployment's capabilities before any stage
  (`asap_planner::SelectionError::InvalidDeploymentCapabilities`).
- Each candidate is then either invalid or valid, recorded as a `Rejection`
  with its `cause` (`RejectionCause`) and reason: a failed check
  (`AccuracyTargetMissed`,
  `CountMinWeightsNotProvenNonNegative`,
  `NoAccuracyGuarantee`, `LatencyBoundExceeded`, `LatencyOverRepeatInterval`
  (a repeating query's latency per evaluation is over its repeat interval,
  checked with or without a latency bound), or `OverClusterCapacity` under
  cost model 3) or a loss under the cost model (`Costlier`, `Slower`,
  `MoreMemory`, `TiedLostOnEnumerationOrder`). Stage 3 never resizes a summary: a candidate
  whose Stage 1 sizes miss its composed accuracy target is rejected alone.
- Usage (`ResourceUsage`) is measured without prices; the `CostModel` in
  `CostInputs` turns it into a choice. Ties keep enumeration order
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
| What the executor can build, retain and how it is priced | the deployment, through `PlanningModels` (`DeploymentCapabilities`, `CostInputs`, `AccuracyModel`) |
| Running the plan | the executor (`asap-executor`), outside the planner |
