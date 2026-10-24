# Joint Planning Implementation Plan

Status: implementation in progress. Logical memo construction, candidate connections, physical configuration selection, and evaluator-backed refinement are implemented. The candidate-connection route is explicitly selectable through the stage driver and CLI; it is not yet the default or a complete replacement for legacy search.

## Outcome

Replace the legacy whole-workload search with one memo-based planning pipeline. Build logical group identity before Stage 1, generate candidates into those groups, expose reusable state through state groups, enumerate executable physical candidates, and select a connected execution DAG jointly. The finished production pipeline must not call or fall back to legacy search.

This is a migration of supported behavior, not just a solver replacement. Keep existing accuracy semantics, physical eligibility checks, cost objectives, scalar outputs, and execution export valid. New runtime algorithms and scheduling are outside scope. No latency or speedup target is claimed before benchmarks.

## Starting gaps (before this migration)

Current `run_stage1_to_stage3_with` generates `PerComputationCandidates` before memo construction. `groups::memo_of` constructs identity and fills candidates together. Shared builds and panes use `CouplingSite` choices; MILP `Choice.copy_of` largely represents all-shared or per-query copies. The proposed producer-state groups and arbitrary partial-sharing connections are not implemented.

Legacy is currently more than a selectable algorithm. The stage driver and public exports live under `search/legacy`; memo clustering uses its source-component and renumbering helpers; the MILP baseline calls legacy planning. Unsupported cost objectives and exhaustive requests also route there. These dependencies must be removed before deleting the directory.

## Naming and ownership

Names should identify domain concepts without requiring readers to decode mathematical notation. Do not introduce synonyms for the same concept or rename unrelated identifiers in bulk. The following are proposed responsibilities, not a requirement to create a new type for every row:

| Concept | Human-readable name | Responsibility |
| --- | --- | --- |
| Logical identity | `MemoGroup`, `MemoGroupId` | Requested result or reusable state contract. |
| Logical computation choice | `LogicalCandidate` | Rule-generated computation with logical dependencies. |
| Executable choice | `PhysicalCandidate`, `PhysicalCandidateId` | Supported operators, configuration, materialization, and execution scope. |
| Consumer dependency | `ProducerRequirement` | One required value/state, including distinct left/right dependencies when applicable. |
| Compatible supplier set | `compatible_producers` | Physical candidates satisfying one consumer requirement. |
| Selected dependency | `ProducerConsumerConnection` | `producer`, `consumer`, and `requirement`. |
| Query coverage | `QueryRootAssignment` | Which selected candidate supplies a query output. |
| Evaluation demand | `QueryEvaluationSchedule`, `ExecutionDemand` | Frequency/alignment, reuse context, and derived execution demand. |
| Selected graph | `SelectedExecutionPlan` | Selected candidates, connections, root assignments, and validated output. |
| Solver activation | `candidate_selected` | Solver variable associated with a physical candidate. |
| Solver connection | `connection_selected` | Solver variable associated with a compatible connection. |
| Cycle exclusion | `topological_rank` | Dependency rank, never execution time. |
| Event-based demand, if needed | `executes_at_evaluation`, `connection_required_at_evaluation` | Clearly named auxiliaries for query evaluation demand. |
| Independent reference | `enumerate_feasible_plans` | Small-input correctness oracle over the same candidate graph. |

The current `PhysicalCandidate` means a whole-workload DAG. Resolve that collision explicitly: rename that wrapper to `PhysicalWorkloadPlan` (or reuse an existing equivalent type) before using `PhysicalCandidate` for a local choice. Migration must update callers and documentation together. Prefer preserving a clear existing name to adding aliases indefinitely.

Logical memo data and rule expansion belong in `logical-asap-optimization`, which Stage 2 already depends on. Physical candidates and materialization legality belong in `physical-asap-optimization`; prices and selection belong in `plan-selection`. Earlier stages must not depend on the solver crate. Move common driver/report types out of the legacy namespace without adding a new framework crate merely to share names.

## Implementation sequence

### 1. Capture behavior and extract shared infrastructure

Inventory supported frontends, scalar/operator roots, sharing rules, materializations, and all configured objectives. Convert representative current outputs into acceptance fixtures containing legality and expected costs, allowing equivalent plan shapes and numerical ties. Record generation/solve/runtime-validation baselines before changing representation.

Move the stage driver, source-component analysis, query-index remapping, and reusable pane functionality out of `search/legacy`. Preserve public behavior while removing memo code's dependency on legacy utility modules. Extract behavior, not legacy search control.

Acceptance: existing tests pass; every remaining legacy dependency is an explicit migration item. No circular crate dependencies or duplicate authoritative implementations.

### 2. Establish logical groups before Pass 1

Split root interning and group identity from candidate population. Construct the initial memo immediately after frontend logical validation. Preserve filters, windows, accuracy requirements, keyless sharing restrictions, scalar references, and query identity. Pass 1 expands those groups directly, with cache keys including relevant consumer requirements.

Group references remain stable throughout expansion, either directly or through canonical-ID resolution after a proven-equivalence merge. Stage 0 creates only the initial groups: Pass 1 and Pass 2 can create intermediate groups, reuse existing groups, and add candidates. Approximate candidates target a result contract and do not assert exact-value equivalence. Compatibility requirements remain explicit when consumer contracts differ.

Acceptance: group identity exists before candidate generation; identical eligible subexpressions share generation work; P95 and P99 remain distinct; pruning changes candidates without invalidating references. Test literal/scalar queries, multiple consumers, fusion boundaries, and canonical-ID resolution after group merging.

### 3. Expose reusable state and generate physical candidates

Pass 2 expands the memo rather than operating on a fixed set of Stage-0 groups:

| Rule effect | Required behavior |
| --- | --- |
| Discover a shared build, pane chain, or rollup state. | Create or reuse a state group and add consumer candidates referencing it. |
| Prove two existing groups logically equivalent. | Merge their canonical identities, reconcile candidates and references, and invalidate affected caches. Sharing a physical component is not proof of logical equivalence. |
| Fuse or bypass a computation in one candidate. | Change that candidate's dependencies; preserve the group if other candidates or query roots still need it. |
| Remove unreachable memo content. | Garbage-collect only when no retained candidate or query root can reach it. A group absent from one selected plan is not automatically removable from the search space. |

For example:

```text
Before Pass 2:
    G95 = {ExactP95, LocalKLLQuantileP95}
    G99 = {ExactP99, LocalKLLQuantileP99}

After discovering reusable KLL state:
    G_KLL = {BuildKLL(...)}
    G95 += EstimateP95(G_KLL)
    G99 += EstimateP99(G_KLL)
```

`G_KLL` is new or reused; `G95` and `G99` remain distinct, and their local candidates remain available. New groups and candidates feed the rule worklist so later matches can use them. A group merge must reconcile producer requirements, parent references, query roots, and rule-application caches; it must not silently equate different accuracy or state contracts. Stage 3 selects the reachable execution subgraph rather than deleting every unselected group from the memo.

Move shared KLL builds, pane chains, and raw retention into explicit state groups. Consumers acquire producer requirements instead of carrying implicit duplicate producer costs. Preserve scan-widening, rollup, top-k, and fusion semantics; only exposed outputs may serve external consumers.

Stage 2 generates legal local physical candidates with concrete configuration, internal materialization assignments, and finite execution scopes. Generate sparse compatible producer connections. Specify scope/binding distinctions needed for multiple candidates from one group to coexist. Do not enumerate every consumer subset or claim completeness without a bounded generation rule. Any restricted scope inventory must be reported.

Acceptance: P95/P99 can share one compatible KLL candidate, select distinct capacities, or stay local; timing and accuracy incompatibilities prevent connections. A fused result does not accidentally expose absorbed intermediate outputs. Eligibility agrees with the current physical evaluator.

### 4. Implement joint selection and reconstruction

Add candidate selection, root assignments, producer requirements, connection activation, reverse support, acyclicity, and resource-specific configuration exclusion. Permit several selected physical candidates per logical group. Reconstruct the selected graph directly into the supported physical IR, preserving distinct execution scopes and deduplicating only the selected shared candidate.

Build an independent tiny-input enumerator over candidate subsets and connections. It is a test oracle, not the old whole-workload search retained as a fallback. Exercise the actual MILP with the same finite candidates and costs.

Acceptance: MILP and enumeration agree on optimal cost for small graphs; shared producer cost is charged once per owned workload, each required input is present, cycles and unsupported outputs are rejected, and zero-cost candidates cannot remain disconnected.

### 5. Complete workload costs and feasibility

Migrate existing per-node pricing, union-rate logic, and sound term cuts into the new representation. Derive demand from selected consumers. Prefer compact schedule classes or proven rate formulas to materializing every query evaluation event; validate event equivalence before compression. Keep a tiny explicit-event reference for aligned/offset schedule tests.

Implement separate demand and lifetime handling for query-time recomputation, ingestion maintenance, and supported retained state. Own source retention in shared resource candidates. Keep fixed request overhead out of the selection objective but in reporting and applicable budgets.

Preserve all existing objective modes: usage-over-time, provisioned capacity, and cluster capacity, including its configured latency/memory priorities and Pareto behavior. Pareto output requires appropriate repeated solves or frontier exploration, not one arbitrary weighted objective. Encode valid accuracy/latency bounds directly where available; retain evaluator refinement with sound local cuts and full-assignment rejection only as a fallback. An unresolved objective cannot silently route to legacy or disappear from the API.

Acceptance: objective and resource accounting match the configured evaluator; shared/repeated execution costs match explicit-event examples; all existing supported objective modes and materializations have acceptance coverage. No unsupported retained-state semantics are inferred from query-time equations.

### 6. Replace production entry points and delete legacy

Build a validated feasible baseline from the same candidate graph, using bounded constructive or feasibility search; do not call legacy to obtain it. Return the best validated incumbent under the planning budget. If none exists, return an explicit no-plan/timeout status rather than relaxing requirements. Budget generation, model construction, solve, and validation, with distinct reporting of solver timeout and proven infeasibility.

Switch the production driver to the new memo pipeline. Remove `SearchAlgorithm::Legacy`, `RoutedToLegacy`, legacy CLI modes, implicit cost-model/exhaustive fallback, and legacy baseline calls. Replace any retained diagnostic enumeration with the new candidate-graph enumerator and explicit size limits. Delete `search/legacy` after its still-needed functionality has moved; remove temporary adapters and old exhaustive selection machinery.

Update stage-pipeline tooling, fuzz entry points, reports, serialized configuration handling, examples, and tests. Obsolete options should receive a clear migration error, not silently select different behavior. No permanent legacy feature flag is part of the final design.

Acceptance: repository search finds no active legacy algorithm, fallback, imports, or user-facing selection mode. Production tests cover all migrated objectives. New exhaustive oracle and recorded acceptance fixtures replace legacy-based differential tests.

### 7. Measure scalability and tune

Benchmark little-sharing workloads, many consumers of few states, dense compatibility graphs, many window/configuration combinations, and evaluator-heavy constraints. Report candidate generation time, model construction time, cumulative MILP solve time, validation time/refinement rounds, peak planner memory, validated execution cost, and total planning wall time. Include solver status/gap and inventory restrictions.

Compare against recorded pre-migration measurements and independent per-query plans generated by the new pipeline. Investigate candidate count, connection density, demand representation, and weak cuts before adding more solver machinery. Keep runtime execution measurements separate from modeled execution cost.

Acceptance: correctness and feasibility hold under time limits; no unbounded event expansion or consumer-partition enumeration is hidden in the default path. Publish measured tradeoffs without assuming the richer model always plans faster.

## Delivery policy

Use reviewable commits following the sequence above. Temporary comparison against legacy during migration is permitted only before the final replacement; the delivery is not complete until legacy production code and fallback have been removed. Do not delete an objective or supported behavior merely to make the migration smaller. Keep the scope and any deferred capability explicit at each checkpoint.

The first implementation change should establish the stage/memo ownership boundary and its regression tests, not introduce a second permanent standalone solver. Subsequent changes should attach to that pipeline. Code names, errors, reports, and tests must use the same producer/consumer vocabulary as the design.

## First draft status

- Implemented: `LogicalMemo`, `MemoGroup`, and `MemoGroupId` in the logical optimization crate, independent of solver types.
- Implemented: `CandidateMemo` and Pass 1 candidate population now belong to logical optimization; population rejects inventories with incompatible roots or invalid target references. Selection retains only a temporary forwarding adapter and pricing context.
- Implemented: initialize logical groups before Pass 1 on memo routes; populate those groups after capability pruning; apply existing Pass 2 rules afterward. The from-Pass-1 API remains supported.
- Implemented: consumer-independent Pass 1 rules are cached by logical group and measure; per-occurrence HAVING, nullable pruning, fusion, and absorption remain contextual. Shared-summary Pass 2 rules now live in logical optimization, with temporary selection reexports.
- Implemented: shared stage orchestration, source-dependency analysis, diagnostic index mapping, and pane reuse moved out of the legacy namespace.
- Implemented: an internal fixed-context candidate-connection MILP core, including same-group multi-selection, additive resource budgets charged once per selected candidate, and an independent tiny-graph enumerator.
- Implemented: opt-in concrete summary-state groups retain local candidates and expose shared builds with explicit reader dependencies. Equal TimeRange data over the same canonical source can feed one concrete state without merging the result groups. Cross-capacity binding and complex producer shapes remain restricted.
- Implemented: typed selected-connection reconstruction preserves candidate identity and ordered scalar/operator dependencies, then recomposes guarantees. Stage 2 must still assign legal materialization and timing on the selected graph.
- Implemented: a restricted memo-to-connection inventory bridge with explicit rejection of unsupported constraints and typed producer bindings. A P95/P99 test selects one shared KLL using synthetic fixed costs, reconstructs the graph, and passes Stage 2 physicalization. This is not a runtime-cost optimality result. The production driver still uses the existing selector; workload-dependent demand, costs, and full feasibility adapters are required before replacement.
- Verified: initial group identity survives Pass 1 population and summary pruning; scalar/literal roots retain query positions; distinct quantile results remain distinct groups.
- Pending: Pass 1 rule generation directly into memo groups, shared-state contracts, arbitrary candidate connections, complete cost-model migration, a strict planning-time budget, and deletion of legacy search/fallback. The baseline now tries at most 16 local memo assignments and three materialization policies per query, without calling the legacy route or materializing all combinations. A strict wall-clock budget across generation and evaluators remains pending.

The current first draft preserves legacy runtime behavior while preparing its removal. It must not be described as completion of the full implementation plan.

## Execution-context integration progress

The follow-up implementation adds selected-output reach constraints, distinct-query reader counts, and a finite configuration inventory with query/ingestion timing and supported summary materializations. It evaluates eligibility for selected readers rather than intersecting all potential consumers. Non-summary highest-shared materialization and implementation rewrites that change candidate identity remain unsupported by this bridge.

Execution-rate accounting is shared with the existing evaluator. A selected-graph demand reference preserves materialized consumer identities, per-request retained-state rates, scalar output references, and ingestion/query timing boundaries. Concrete positive accuracy-budget configurations are admitted; root guarantee recomposition and Stage 3 validation decide whether their composition is accurate enough.

A bounded refinement loop now reconstructs configurations through Stage 2 and prices them with `PlanSelector` under usage and provisioned-price objectives, with a frozen calibration fit. It retains a validated incumbent and reports its bound, gap, and stopping reason. Exact observed full-assignment cost cuts include absent as well as selected decisions; only proven rejected assignments are excluded. Internal or unsupported reconstruction failures remain diagnostics.

The standard stage driver and `stage_pipeline --search candidate-connections` now expose this path explicitly. Its report identifies the restricted catalog (Pass 1 and concrete shared summaries), defaults of 128 evaluator refinements and 30 seconds, and any stopping reason. It reports memo expansion, physical inventory construction, model construction, solver and validation time, refinement rounds, and peak MILP variables/constraints. Memo expansion timing excludes frontend lowering and Pass 1 generation. A no-incumbent result includes the report in its diagnostic; it does not silently use legacy.

The initial zero lower bounds and full-assignment cuts are a correctness foundation, not a scalability claim. Existing routes remain available because the new route does not yet cover pane/rollup/scan-widening expansion, all fusion and physical rewrites, or cluster lexicographic/Pareto objectives. Default production replacement, complete rule coverage, constructive baseline/budget integration, stronger contextual bounds/cuts, and deletion of legacy search remain required before this migration is complete.
