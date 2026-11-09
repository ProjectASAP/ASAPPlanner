# ASAP-Aware Mapping contracts

Audience: developers changing the planner. This reference states the rules
the stage code relies on: what each stage's input and output may contain,
and which component owns each decision. Read the
[architecture](asap-aware-mapping-architecture.md) first; use the
[extension guide](extend-asap-aware-mapping.md) when changing one. Field-level
details are in the rustdoc (`cargo doc -p asap-stage0-memo -p asap-stage1-pass1-per-computation-candidates -p
asap-stage1-pass2-reuse-candidates -p asap-stage2-physical-optimization -p asap-stage3-plan-selection -p asap-types
--no-deps`).

## 1. IR at each stage

| After | The DAG holds | Timing |
| --- | --- | --- |
| Stage 0 (frontends) | `NonASAPOp` nodes only; aggregates carry `AggIntent` measures with each query's `AccuracyTarget` as written | none |
| Stage 1 (Pass 1 and Pass 2 rules) | `ASAPOp` nodes where a candidate chose an exact accumulator or a summary (`SummaryAgg`, `SummaryTumblingWindowChain`, `SummaryMerge`, `SummaryEstimate`, `FinalizeExactAccumulator`); everything else stays `NonASAPOp` | none: Stage 1 never decides when anything runs |
| Stage 2 (`generate_physical_asap_memo_candidates`) and Stage 3 (`select_plan`, which builds the plan) | the same operators, with a timing and a `Lifetime` on each summary build | every node of the plan timed from the physical candidate it was built from (`selected_physical_candidate`) |

- Structurally equal sub-DAGs are interned before Stage 1
  (`cse.rs` in `asap_stage0_memo`), so Stage 1 compares nodes by structure and Pass 2
  matches consumers structurally, never by pointer.
- Timing is applied with `apply_physical_properties`, which validates
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
  ways to compute list the exact option first (`ExactAccumulator` when the
  intent has a mergeable exact state, else `Exact`), then each
  `Summary(algorithm)` the deployment supports. Candidates are unranked:
  Pass 1 never compares cost.
- **Pass 1 adds what it can compute.** A way to compute a measure that
  cannot exist (a summary the deployment does not support, a top-k heap over
  one row per item) is not added; there is no Pass 1 pruned list.
- **One candidate per θ.** A summary state group holds one candidate per
  θ of its summary type's parameter list; θ is structure only (no ε or δ).
- **Fused computations.** `FusedComputation { absorbed_targets, ways_to_compute }`
  replaces the targets beneath (the whole-expression top-k, the nested sum).
  It absorbs the target directly beneath and, for a heap over the raw data samples
  beneath a grouped sum of per-series sums or counts, the per-series target
  too.
- **Pass 2.** Each rule adds candidates or groups; none copies a consumer,
  adds a summary type the deployment does not support, or adds a readout.
  Each readout gets its required accuracy (`LogicalRequirements`) from the
  queries reaching it; a candidate no logical edge can use and that is no
  query output is eliminated (`LogicalASAPMemoCandidates::pruned`, each
  with one `PruneCause` per failing edge: the eliminated consumers it comes
  through and its `RootPruneReason`).

## 3. Accuracy

- A summary's guarantee is the deployment's measurement
  (`AccuracyEvidence::measured_guarantee`) when there is one, else its
  algorithm's error formula (`theoretical_guarantee`); Pass 2 compares it
  with each readout's `RequiredAccuracy`
  (`producer_satisfies_logical_requirements`). It fails closed: no rule
  means no guarantee, and the edge is not admissible.
- A target with no `accuracy_metric` accepts a guarantee in any metric that
  meets its ε, δ and scope; Stage 3 reports the metric it accepted
  (`AcceptedAccuracy`).
- A consumer passes its required accuracy to each input through its error
  contract (`LogicalRequirements::error_propagation`); Stage 3's constraint
  (K) allocates a composed query's error among its readouts.
- Facts come only from the declared `DataWorkload`; an undeclared fact is
  unknown, never zero.

## 4. Stage 2

- Every logical candidate Pass 2 kept is expanded into one
  `PhysicalCandidate` per valid (timing, lifetime) (`PhysicalProperties`),
  and every logical edge into physical edges with $\alpha^{\log}$ and
  $\alpha^{\mathrm{phys}}$.
- On the memo (`generate_physical_asap_memo_candidates`), every physical
  candidate Stage 2 prunes is recorded in
  `PhysicalASAPMemoCandidates::pruned_candidates` as
  `PrunedCandidate<PhysicalCandidate>` with a `PruneReason`: an
  ingestion-time alternative without an ingestion pipeline
  (`NoIngestionPipeline`), a query-time scan of raw data the deployment does
  not retain (`RawDataNotRetained`) and a materialized query-time result the
  deployment does not retain (`QueryTimeResultsNotRetained`) are not
  generated; a candidate no buildable input or query output can use is
  removed (`InputHasNoBuildableProducer`, `ReachesNoQueryOutput`). The
  planner does not check executor support. Stage 2 emits one candidate per operator node,
  timing and lifetime; it makes no execution-scope copies.

## 5. Stage 3

- `select_plan` validates its inputs first: `ErrorSelectingPlan::InvalidSettings`
  and `EntryCount` are bad input and fail the whole selection. The facade
  validates the deployment's capabilities before any stage
  (`asap_planner::SelectionError::InvalidDeploymentCapabilities`).
- Before its selection MILP, Stage 3 prunes a candidate another
  dominates (`DominatedBy`) or that cannot be costed (`UsageNotCostable`),
  recorded in `Selection::pruned_candidates`. The MILP selects executions
  and edges over execution contexts; constraint (K) bounds each query's
  composed error at its root. A workload with no plan fails with
  `ErrorSelectingPlan::NoValidCandidate` (`RejectionCause::Infeasible`).
- Usage (`ResourceUsage`) is measured without unit costs; the `CostModel` in
  `CostInputs` turns it into a choice. The selection MILP implements cost
  model 1 only.
- Stage 3 builds the plan, a `PhysicalASAPDAG` with each node's θ and
  retention; its cost is `asap_cost::plan_cost` of the plan.

## 6. Ownership

| Decision | Owner |
| --- | --- |
| What a query means | the frontend (Stage 0) |
| Which computations exist and how each may be computed | Pass 1 |
| What is shared, read from a chain of tumbling windows or reused | Pass 2 |
| When each build runs and whether it is kept | Stage 2 |
| Which plan is valid and which is chosen | Stage 3 |
| What the executor can build, retain and how it is costed | the deployment, through `PlanningModels` (`DeploymentCapabilities`, `CostInputs`, `AccuracyEvidence`) |
| Running the plan | the executor (`asap-executor`), outside the planner |
