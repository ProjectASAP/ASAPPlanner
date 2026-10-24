# Joint Planning Implementation Plan

**Status:** Source migration implemented; final acceptance is in progress. The working branch has one candidate-connection production route. The legacy planner, old memo selector, algorithm fallback, and temporary comparison entry points have been removed. This does not establish release readiness: full regression, CI, and workload cost/planning-performance acceptance remain required.

## Outcome and scope

Build logical group identity before Stage 1, generate computation candidates, expose reusable state, enumerate executable physical candidates, and jointly select their dependency connections. `CandidateConnections` is the sole search engine. Exhaustive diagnostics, bounded search, and feasible seeds use that engine; none calls the retired planner.

Preserve supported accuracy semantics, physical eligibility, cost objectives, scalar outputs, and execution export. New runtime sketch/window algorithms and scheduling are outside scope. Complete and validate this MILP implementation before considering DP compression. A richer candidate space does not by itself establish faster planning or better timeout incumbents.

## Current architecture and ownership

| Scope | Current implementation | Responsibility |
| --- | --- | --- |
| Logical identity | `logical-asap-optimization/src/memo.rs` | `LogicalMemo`, `MemoGroup`, stable initial query/group references. |
| Computation and sharing rules | `logical-asap-optimization/src/memo/{candidates,rules,state_groups,state_configurations,window_candidates}` | Populate groups, retain local candidates, expose shared states and compatible rewrites. |
| Physical legality | `physical-asap-optimization` | Executable operators, timing, materialization eligibility, and `PhysicalWorkloadPlan`. |
| Candidate dependency inventory | `plan-selection/src/search/candidates` | Local `PhysicalCandidate`, `ProducerRequirement`, compatible suppliers, execution scopes, selected connections, and reconstruction. |
| Solver | `plan-selection/src/search/milp` | Activation, producer/root assignment, reachability, acyclicity, resource constraints, and solver warm starts. |
| Search and validation | `plan-selection/src/search/refinement` | Actual evaluator checks, sound cuts, objective-specific ranking, certified component splitting, and structural pruning. |
| Pricing support | `plan-selection/src/search/costing` | Certified local bounds and consistent calibration. Production execution demand is owned by `plan-selection/src/cost/execution_demand.rs`; the search demand reference is test-only. |
| Pipeline and lifecycle | `plan-selection/src/search/{pipeline,route}` | Initial groups, generation accounting, seed/full search budgets, archives, reports, and selected workload outputs. |

`PhysicalCandidate` is a local selectable computation. `PhysicalWorkloadPlan` is a complete Stage 2 workload DAG; they are not interchangeable. Physical candidate configuration in selection invokes the authoritative Stage 2 legality code rather than duplicating its rules. Logical rules do not depend on the solver crate.

Pass 1 still exposes the supported `PerComputationCandidates` interface and populates the already-created memo. This is not a second workload selector. Pass 2 can add intermediate state groups and consumer candidates; sharing a component does not merge distinct requested results:

```text
G95 = { ExactP95, LocalKLLQuantileP95, EstimateP95(G_KLL) }
G99 = { ExactP99, LocalKLLQuantileP99, EstimateP99(G_KLL) }
G_KLL = { supported KLL build configurations }
```

Multiple physical candidates from `G_KLL` can coexist. Their selected connections determine sharing subsets, dependencies, and execution demand. Only exposed outputs can supply another consumer; a fused result does not automatically expose absorbed intermediates.

## Capability and acceptance gates

Implemented means present in this branch, not that every performance gate has passed.

| Capability | Current behavior | Acceptance requirement |
| --- | --- | --- |
| Exact, accumulator, sketch, fused, and scalar outputs | Supported local generation and selected-graph reconstruction; strict schema and guarantee checks. | Existing frontend/composition regressions, including SQL COUNT qualifiers and AVG grouping-key proofs. |
| Shared builds, pane states, scan widening, rollup, and TopK reuse | Logical rules expose compatible shared computations and retain local choices. | Typed input/output contracts, selected-reader legality, and actual evaluator-cost checks. Unsupported operator shapes remain local. |
| Capacity, width, and horizon configurations | Finite sketch capacities and retention configurations; complete legal integer-ms pane-width domain for the supported aligned whole-pane algorithm. | Small-domain completeness, duplicate-generation equivalence, alignment, coverage, and independent solver-oracle tests. |
| Materialization and repeated execution | Supported summary and non-summary modes validated against selected connections; production evaluator derives execution demand and retained state. | Ingestion/query timing, different evaluation grids, retained-state freshness, and shared cost ownership. |
| Accuracy | Root guarantees are recomposed and checked; proven query-closure cuts and conservative early pruning supplement full-assignment rejection. | Preserve failure-probability and value/rank-error semantics. A changed relevant input must remain able to repair a rejected plan. |
| Cost bounds | Certified local bounds plus validated full-assignment cost observations. Unsupported bounds remain conservative. | No bound exceeds evaluator cost, including potential candidate cycles, grouped panes, retention, and sharing. |
| Objectives | Usage-over-time, provisioned-price, and cluster-capacity latency/memory/Pareto ranking use the configured evaluator. | Same feasibility and ranking as independent final evaluation. Bounded Pareto traversal must report incompleteness. |
| Calibration | Explicit frozen factor, no-measurement default factor, or complete-catalog fit. | No fitting to an arbitrary timeout-dependent prefix; imported measurements are not newly collected runtime data. |
| Search controls | Structural pruning, certified independent components, warm starts, validated seed, and cooperative budgets. | No removed global constraints or lost sharing opportunities; timeouts cannot become optimality certificates. |
| Production cutover | One engine and one public lifecycle; retired algorithm names are rejected. | No active legacy selector/fallback/imports; CLI, fuzzing, viewer, and exports use the same route. |

Raw-source retention and fixed request overhead are currently accounted for by the authoritative workload evaluator. Explicit `G_raw` candidates in the design are a possible direct encoding, not a second implemented retention owner. Never charge both representations for the same resource.

## Complete pane widths and bounded planning

Query window semantics remain fixed. Complete width generation enumerates the legal positive integer-millisecond divisors of the relevant window/evaluation grids, subject to alignment, offsets, representable pane counts, and executable configurations. Different consumer subsets may select different widths. Complete width coverage does not imply arbitrary sketch capacities or unsupported window algorithms.

For bounded searches with a fixed calibration model, the same engine first searches a reader-grid/GCD seed domain, using at most the smaller of 5 seconds and one quarter of the available wall budget. It then searches the complete domain with the remaining wall and evaluation budgets. The seed is accepted only after normal accuracy, materialization, latency, and cost validation. The two results use the same configured ranking; a restricted seed supplies no lower bound for the complete domain.

For accuracy-enforced feasibility proposals, the seed avoids rank-only or unknown estimate contracts without a rank request, optional non-summary materializations whose eligibility depends on the selected consumers, and projected raw-source bindings that cannot be installed below a time range. Projections above an already formed range remain available. These are proposal restrictions, not proofs or permanent pruning: the complete graph retains those candidates, and an infeasible restricted proposal falls back to full selection. Ignored-accuracy cost proposals retain shared non-summary materializations.

Exhaustive/keep-every diagnostics and complete-catalog calibration use a single complete domain. A complete-catalog fit cannot be replaced by a partial seed fit. If no validated plan exists when the budget ends, return an explicit no-plan diagnostic. Internal generation/reconstruction errors propagate rather than silently choosing a seed.

The deadline is cooperative: generation loops, inventory/configuration construction, and MILP construction check it; an individual evaluator or external solver call may finish after its allowance. Reports distinguish seed time, complete-phase time, full catalog construction, assignment-search completion, selected seed/fallback reason, and solver status/gap. A fallback clears restricted-domain bound/gap claims. `full_catalog_completed` does not mean every assignment was evaluated.

## Delivery stages

| Stage | Deliverable | Exit criterion |
| --- | --- | --- |
| 1. Memo and candidate migration | Initial groups, local/shared rules, physical contracts and scope-aware connections. | Supported outputs remain representable; logical identities and ordered dependencies are preserved. |
| 2. Joint optimization and evaluator parity | One MILP selector, all objective modes, shared-cost ownership, and accuracy/materialization validation. | Tiny cases agree with exhaustive enumeration under the same model; real evaluator constraints hold. |
| 3. Bounded complete-width search | Full legal width generation, safe pruning/bounds, seed fallback, and complete reporting. | Generation/model deadlines work; retained incumbents are valid; restricted runs make no full-domain optimum claim. |
| 4. Acceptance and publication | Full regression, exact-head CI, ASAPQuery workload results, final source/docs audit, and PR publication. | One implementation remains; all required checks pass and unresolved performance/cost regressions are reported or fixed. |

The source cutover is already present on the working branch. Publication as a completed migration remains gated by acceptance, not by deletion alone. Do not restore a legacy fallback to make a failing workload pass, and do not delete an objective or weaken a guarantee to reduce the migration scope.

## Validation and measurement

Use the independent small-graph candidate/connection enumerator as a solver oracle. Test partial sharing, several selected candidates in one group, independent input positions, selected-root coverage, cycles, zero-cost orphan exclusion, and capacity limits. Retain the existing accuracy and materialization regressions; changes in candidate order must not substitute for semantic assertions.

Run the ASAPQuery viewer manifests and record coverage failures, rejected guarantees, validated cost, planning time, and stopping status. Compare with recorded pre-migration checkpoints and independent per-query plans from the current engine. Preserve failed checkpoints instead of reporting only the fastest successful runs.

Report candidate generation, model construction, cumulative solver time, validation time/refinement rounds, peak planner memory, total wall time, candidate/connection/model sizes, and validated execution cost. Avoid double counting the seed/full phase timers against their constituent timings. Modeled execution cost is not measured runtime execution cost. A speedup caused by returning no plan is a failure, not an optimization.

See [workload evaluation](../../develop_docs/joint-candidate-workload-evaluation.md) for measured checkpoints and remaining acceptance findings. Final full-workspace tests, strict linting, CI, and workload quality checks determine readiness; this plan does not claim them completed in advance.
