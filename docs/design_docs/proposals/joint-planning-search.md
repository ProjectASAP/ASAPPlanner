# Joint planning search: exact reductions, bounds, and heuristic fallbacks

Status: proposal; no new pruning rule is implemented by this document. Audience: planner designers and implementers.

ASAPPlanner must search a workload's candidate plans without retaining or pricing every combination. The design keeps the decisions in [planner-layering.md](planner-layering.md), and changes how those decisions are explored. It builds on the [optimizer survey in #671](https://github.com/ProjectASAP/ASAPPlanner/issues/671#issuecomment-6137638876).

## Contents

- [Problem and scope of the guarantee](#problem-and-scope-of-the-guarantee)
- [Survey of optimizer search approaches](#survey-of-optimizer-search-approaches)
- [Implications for ASAPPlanner and proposed approach](#implications-for-asapplanner-and-proposed-approach)
- [Search representation and interaction analysis](#search-representation-and-interaction-analysis)
- [Exact pruning rules and their proof conditions](#exact-pruning-rules-and-their-proof-conditions)
- [Bounds and branch-and-bound](#bounds-and-branch-and-bound)
- [Composition under the three cost models](#composition-under-the-three-cost-models)
- [Heuristics and what they give up](#heuristics-and-what-they-give-up)
- [Search procedure and stopping contract](#search-procedure-and-stopping-contract)
- [MILP as a relaxation and an alternative search](#milp-as-a-relaxation-and-an-alternative-search)
- [Validation and implementation order](#validation-and-implementation-order)
- [References](#references)

## Problem and scope of the guarantee

A motivating measurement was a growing prefix of one 10-query ASAPQuery mixed workload:

| Queries | Planning time | Peak memory |
| ---: | ---: | ---: |
| 3 | 0.4 s | 0.14 GB |
| 4 | 5 s | 1.5 GB |
| 5 | 129 s | 24.7 GB |
| 6 | stopped around 228 s | around 100 GB |

These are motivating observations, not performance guarantees for this proposal. The Cartesian product of per-target alternatives, subsets of sharing opportunities, and materialization assignments makes enumeration exponential. Streaming reduces retained candidates; it does not remove that time complexity. No exact algorithm here promises seconds on every workload.

Let `F` be the finite set of complete plans the reference generator admits for a fixed workload, rule inventory, parameter inventory, deployment capabilities, and evidence snapshot. For scalar minimization, exact search returns a feasible `P*` with `C(P*) = min(C(P) for P in F)`. For a Pareto request, it returns the complete nondominated objective set, with explicitly defined handling of equivalent plans.

This guarantee has three separate boundaries:

- **Search completeness:** no plan in `F` that could improve the requested result is lost. It says nothing about rewrites, sketch parameters, or window realizations absent from `F`.
- **Model optimality:** `C` is the configured planner objective. An exact search can still choose a slow runtime plan when its cost or cardinality model is inaccurate. Empirical estimates are inputs to that model, not measured-runtime guarantees.
- **Validity:** accepted plans must satisfy the existing semantic, output-accuracy, capability, and executability contracts. An empirical accuracy acceptance is exact relative to that declared evidence policy, not a new theoretical error guarantee.

Fix query recurrence, time ranges, pricing, measured costs, accuracy evidence, and tie handling for a search. A changed model invalidates cached costs and proof certificates. Nonfinite or unknown estimates never justify pruning. Unsupported cost models fall back to a weaker bound, including zero for a nonnegative scalar objective.

**Tie contract:** preserving optimal cost need not preserve the same candidate ID. Define a deterministic total ranking for the new search, with stable reference rank for exact ties. The legacy relative-tolerance comparison is not transitive; stable IDs alone cannot reproduce its sequential winner after reordering. If legacy winner identity is required, replay the reference-order comparison for all potentially winning near-tie candidates, or keep that path exhaustive until an equivalent rule is proved. A Pareto report must say whether it preserves all equivalent plan identities or only one representative per objective vector. Changing the tie policy is a compatibility decision, not an exact-search optimization.

## Survey of optimizer search approaches

The approaches below differ in what they enumerate, what they memoize, and which alternatives they discard. Their guarantees apply to their specified plan spaces and cost models. This survey presents those mechanisms before choosing how to use them in ASAPPlanner.

### Bottom-up dynamic programming: System R, DPccp, and DPhyp

[System R](https://research.ibm.com/publications/access-path-selection-in-a-relational-database-management-system) builds plans for progressively larger relation subsets. For each subproblem it retains cheap alternatives and useful physical properties, notably interesting orders that can reduce later sorting costs. The classic search concentrates on left-deep join plans. The recurrence relies on optimal substructure: once the subset and relevant properties are fixed, a more expensive equivalent subplan need not be reconsidered in every parent.

[DPccp](https://www.vldb.org/conf/2006/p930-moerkotte.pdf) enumerates connected subgraphs and their connected complements to generate bushy join trees without cross products. [DPhyp](https://15721.courses.cs.cmu.edu/spring2020/papers/20-optimizer2/p539-moerkotte.pdf) extends the graph treatment to hypergraphs and more complex join predicates. These methods reduce enumeration work by exploiting query structure; they do not make arbitrary plan spaces polynomial. Their exactness is relative to the join alternatives and properties they retain.

### Top-down memoization: Volcano and Cascades

[Volcano/Cascades](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf) organizes equivalent expressions into memo groups. Transformation rules add equivalent expressions; implementation rules supply physical alternatives. Optimization requests include required properties, so the same logical group can have different winners for different contexts. A parent requests child implementations, combines their costs, and can stop exploring an alternative when a valid cost limit rules it out.

The memo avoids optimizing the same subproblem repeatedly. Branch-and-bound avoids completing alternatives that cannot improve a known result. These mechanisms depend on sufficient property keys and valid bounds; storing equivalent expressions together does not itself decide whether their runtime results should be computed once and shared.

### Ordered rule passes: DataFusion

[DataFusion's optimizer documentation](https://datafusion.apache.org/library-user-guide/query-optimizer.html) describes logical optimizer rules, configurable rule sets, and repeated passes. Rules simplify expressions, push predicates and projections, and rewrite plan structure. Physical optimization is a separate phase concerned with executable plans and their requirements.

Rule application can reduce work without explicitly searching every equivalent plan. A pass limit or fixed point describes when rewriting stops; it is not a certificate that the globally cheapest plan has been examined. Correctness of each rewrite and completeness of cost-based search are separate properties. A rule pipeline can also serve as preprocessing for an enumerator or memo-based search.

### Joint sharing and materialization: multi-query optimization

[Multi-query optimization by Roy et al.](https://arxiv.org/abs/cs/9910021) extends optimizer reasoning to common subexpressions across queries. A shared expression can be built once, materialized, and read by several consumers. Its benefit depends on the readers, their alternatives, and the cost of materialization and reuse. The paper presents Volcano-based and greedy cost-based heuristics rather than a general exhaustive-optimum guarantee.

[Materialized-view selection in data cubes](https://dsf.berkeley.edu/cs286/papers/datacube-sigmod1996.pdf) studies which aggregate views to retain under a space constraint, exploiting relationships between views in a lattice. Its greedy results rely on that problem's benefit and constraint structure. General workload DAGs can have overlapping sharing opportunities and incompatible configurations, so the same approximation guarantee requires a separate proof.

### Mathematical optimization: MILP

[Trummer and Koch](https://arxiv.org/abs/1511.02071) encode join-order and implementation decisions as binary variables with constraints restricting legal plans. Their formulation searches left-deep plans and approximates operation costs with linear functions. A solver can return a feasible incumbent, a bound, and an optimality gap for the formulation.

This separates the combinatorial search from its implementation, but the encoding must cover the intended alternatives and resource constraints. Solver optimality concerns the encoded objective. Cost approximations and omitted physical choices determine how that result relates to the original planning problem.

### Bounded and randomized search

Greedy choice, local improvement, beam search, and randomized exploration limit the planning effort spent on large spaces. [PostgreSQL documents GEQO](https://www.postgresql.org/docs/17/runtime-config-query.html#RUNTIME-CONFIG-QUERY-GEQO) as a heuristic alternative for large join problems that can produce plans inferior to exhaustive search.

These techniques can find useful plans quickly. Used only to order a complete search or provide feasible incumbents, they do not remove its eventual optimality guarantee. Used to discard alternatives or stop without a certificate, they trade completeness for planning time.

## Implications for ASAPPlanner and proposed approach

| Approach | Useful idea for ASAPPlanner | Exactness boundary |
| --- | --- | --- |
| System R-style DP; DPccp/DPhyp [1](https://www.vldb.org/conf/2006/p930-moerkotte.pdf) | Memoize subproblems; exploit sparse interaction graphs; retain properties a parent needs. | A single cheapest subplan is safe only with optimal substructure. DPccp's cross-product-free join space is a restriction, not proof that arbitrary workload sharing is separable. |
| Volcano/Cascades [2](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf) | Groups of equivalent computations, required physical properties, cost limits, and memoized exploration. | A memo shares optimizer work. It does not automatically decide runtime sharing, materialization, or a correct state key. |
| MQO and view selection [3](https://arxiv.org/abs/cs/9910021), [4](https://github.com/ProjectASAP/ASAPPlanner/issues/671#issuecomment-6137638876) | Model build-once/read-many decisions jointly; use greedy materialization as a practical incumbent. | Greedy MQO is heuristic. Approximation guarantees for a particular data-cube lattice or benefit function do not transfer to arbitrary ASAP DAGs. |
| MILP [5](https://arxiv.org/abs/1511.02071) | Explicit coupled decisions, relaxations as lower bounds, solver incumbents and gaps. | Exact only for the encoded choices and objective. Linear cost approximations and omitted sharing modes can change the optimum. |
| Rule passes and bounded/randomized search | Cheap simplification, branch ordering, and a predictable planning budget. | Restricting exploration is heuristic unless omitted alternatives are proved dominated. PostgreSQL explicitly documents this tradeoff for GEQO [6](https://www.postgresql.org/docs/17/runtime-config-query.html#RUNTIME-CONFIG-QUERY-GEQO). |

**Decision:** build a streaming, memoized branch-and-bound search with conservative interaction analysis. Enable a pruning rule only when its preconditions establish that it preserves the optimum. Use greedy search to find incumbents and order exploration; discarding alternatives by a heuristic is a separately reported approximation mode. A memo, dynamic programming, or a MILP solver does not by itself make workload sharing exact.

The implementation boundaries are documented in [`stage1_to_stage3.rs`](../../../crates/plan-selection/src/pass/stage1_to_stage3.rs); sharing rules in [`asap_aware_cse.rs`](../../../crates/logical-asap-optimization/src/asap_aware_cse.rs); and resource aggregation and ranking in [`plan-selection/src/lib.rs`](../../../crates/plan-selection/src/lib.rs). Audit those contracts when implementing each rule. The following sections define the search state, pruning conditions, and execution procedure for ASAPPlanner.

## Search representation and interaction analysis

### Memo groups and boundary state

**Memo group** is established database-optimizer terminology: a group of logically equivalent expressions in the optimizer's memo. See [Graefe's Cascades paper, Section 2](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf) and [CockroachDB's optimizer documentation, “Memo”](https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/opt/doc.go#memo). Here, a group represents a computation with the same observable semantics. Its alternatives describe exact or summary implementations, their output guarantees, and legal physical realizations. Similar source names, matching hashes, or equal output column names do not establish equivalence.

A search state records the unresolved choices and the interface through which already chosen work can affect them. Include, when observable:

- source identity, predicates, grouping, value/weight semantics, schema and row identity;
- summary family, parameters, composed output guarantees, and consumer requirements;
- evaluation frequencies, aligned windows, offsets, pane layout, and retention horizons;
- ingestion/query phase, retained versus temporary output, storage medium, and required physical properties;
- offered shared producers and consumer bindings, their adapters, and which costs are already counted;
- boundary resource demands and active source-retention horizons needed for later composition.

Consumers with different accuracy targets may need a shared producer sized for all of them. Sharing opportunities can be partial, overlap, or arise after an earlier rewrite. The state cannot be just a query ID, a count of consumers, or a scalar cost.

Two states may reuse a cached optimum only when every future completion observes the same interface and the same cost/evidence context. Hashes only find possible matches; verify structural and semantic equality. A bounded cache may evict entries and recompute later without losing exactness; a bounded beam discards search alternatives and does lose it.

### Conservative interaction hypergraph

Represent decisions as variables, and interactions as factors/hyperedges. Examples are a producer and all its potential readers, a summary shared by three consumers, a raw source's retention charge, and a workload resource constraint. Pairwise projection is sufficient to find connected components if it conservatively includes all members of every hyperedge.

Build potential sharing from **all alternatives and the registered rewrite closure**, not from the independent winner of each query. Include identical expression, capability, rollup, top-k, window/pane reuse, and within-query dependencies. A common metric name is neither necessary nor sufficient: aliases can name one source, and different predicates can prevent sharing.

Each sharing rule must expose a conservative summary of the sources, producers, consumers, and properties it might connect. If the analyzer cannot rule out an interaction, keep the decisions connected. New rules default to “may interact” until their interaction summary is implemented. False positives cost planning time; false negatives invalidate exactness.

Whole-workload factors can sometimes be composed after solving computational components, but are not ignored. For example, raw disk retention is the **maximum** requested horizon per source in the current cost model, not the sum of each query's standalone disk use. Preserve that source interface or join its components. The analyzer must be conservative over transformations that introduce a new compatible window or producer, not merely direct matches in the initial graph.

## Exact pruning rules and their proof conditions

Here “exact” means the stated conditions are satisfied. It is a correctness property, not a promise that a rule removes many candidates or makes the workload fast.

The safest first implementation keeps exhaustive generation semantics and enables only duplicate elimination, completed-plan rejection, and an initially zero lower bound. Those preserve the result without assuming useful sharing savings. Component splitting, local DP, and forced sharing require additional certificates and remain disabled when their preconditions are unknown. Streaming is an execution strategy, not a new pruning theorem.

### E1. Remove duplicate search states and equivalent complete plans

If two states have the same full interface, remaining legal choices, and resource/cost obligations, expand one and reuse its result for the other. Canonical node IDs or traversal order must not affect identity. Preserve alias/provenance information required by explanations and tie handling.

**Proof:** each discarded state has exactly the same completions and costs as its representative. This avoids repeated optimizer work without forcing shared runtime execution. Registering a legal common expression in a memo is different from deciding to materialize it once.

### E2. Reject permanently invalid branches

Prune when no remaining completion can meet semantics, deployment capability, executability, or the query's output-accuracy policy. Examples are a fixed operator with no supported implementation and no remaining replacement, or a fixed unrepairable type error. A fixed raw-data requirement with raw retention forbidden is another example, provided no unresolved materialization can remove the query-time read.

Per-query checks may run early only for this invariant subset. Otherwise carry obligations forward: shared resizing can improve accuracy; a different family can supply evidence; ingestion materialization can remove a raw read; later lowering can change an operator shape. Recheck final shared output guarantees and executor support on completed plans.

Constraint propagation is another exact case: remove a domain value only after proving it has no support in a required constraint, even in a relaxation of the other decisions. An empty domain then rejects the branch. Pairwise arc consistency can be a cheap necessary check, but passing it does not prove that a globally feasible completion exists.

**Proof:** the branch's feasible completion set is empty. Treating absent evidence as rejection of a completed plan can follow the existing policy; treating it as permanent rejection before all relevant transformations is a different and generally unsafe step.

### E3. Prune by an admissible workload lower bound

For a partial state `S`, compute `LB(S)` such that `LB(S) <= min C(P)` over its feasible completions. If a fully validated incumbent has objective `U`, discard `S` when its bound cannot beat the incumbent under the chosen tie rule. The default conservative scalar comparison is strict `LB(S) > U`, with numerical margins described below.

A currently composed partial plan's cost is **not** automatically a lower bound: later sharing can remove work, and later rewrites can change the physical implementation. Only irrevocable, nonnegative charges can be accumulated without a proof of the remainder. The next section defines progressively stronger relaxations.

**Proof:** every completion costs at least the bound, so none improves the incumbent. Prune also when a conservative lower bound on a hard resource exceeds its limit. A sum that double-counts possibly shared memory or CPU is not a valid resource lower bound.

### E4. Replace a state by a contextually dominating state

For every completion of state B, there must be a feasible completion of A that preserves each query's output contract, exposes compatible boundary producers/properties, and is no worse on the requested objective and constrained resources. Then B can be removed. In a complete-plan comparison there is no unresolved context; use the actual whole-plan ranking directly.

A practical sufficient certificate for partial states is the same boundary interface and a resource/cost vector no larger in every coordinate consumed by the monotone composition function. Keep alternatives with different useful orderings, summary capabilities, source horizons, recurrence/retention requirements, or accuracy guarantees unless the substitution proof covers those differences. A cheaper standalone candidate with fewer reusable outputs is not a dominating state.

**Proof:** mapping every potentially winning completion of B to a no-worse completion of A preserves the optimum. Equal vectors are deduplicable only under the declared representative/tie policy. Context-free per-query Pareto pruning is not justified just because several metrics are recorded.

**Example: preserve benefits to other queries.** Query A can use a producer costing 10 plus private work costing 6, or an independent alternative costing 5. Query B costs 100 independently, but a permitted reuse rewrite can answer it from A's producer with 1 unit of adapter work. Keeping the producer gives a joint cost of `10 + 6 + 1 = 17`; removing that opportunity gives `5 + 100 = 105`. A replacement of A must preserve B's access to that producer, or account for the loss in the whole-workload comparison. The producer's own cost is not an upper bound on the saving it enables for other consumers.

### E5. Solve truly independent components separately

If `F = F1 × ... × Fk`, feasibility factors, and `C(P) = sum Ci(Pi)`, choose a scalar optimum independently in each component. Potential interactions must have been analyzed conservatively as above.

When feasibility is globally coupled, keep sufficient component alternatives and combine them through a global resource DP or search. A scalar cheapest plan per component can be infeasible in combination. If source-retention horizons or other properties cross the boundary, retain those properties or merge the components before applying the rule.

**Proof:** the product of independent local optima minimizes the separable sum. With constraints, the same proof applies only after conditioning on a sufficient resource/boundary assignment. Merely reading different metrics does not prove independent capacity constraints.

### E6. Eliminate local materialization choices only behind a complete interface

An unshared build may still share a raw scan, overlap ancestor working memory, change required raw retention, or affect the latency path. Fix its boundary inputs, outputs, lifetimes, and physical requirements first. With no remaining interactions and a scalar separable objective, choose the cheapest legal local realization. With resource tradeoffs or an observable output property, retain the nondominated alternatives for that interface.

For example, local option A costs 1 and uses 10 memory units, while B costs 2 and uses 1. Under a memory limit of 2, discarding B for A is incorrect even when no other query reads this build. A subtree DP is exact only after all externally visible dependencies are represented in its state.

### E7. Force identical-expression sharing only with a physical dominance proof

The existing [`ir/cse` legality checks](../../../crates/types/src/ir/cse.rs) establish which expressions may share without changing results; structural equality alone is insufficient for some unkeyed outputs. They do not prove that every shared physical realization is cheaper.

A sufficient restricted case is a pure deterministic computation with the same inputs, evaluation domain, schema, precision, and required properties; executing it once preserves consumer scheduling, and every relevant build/read/storage/latency/resource charge is no larger. No extra retention, copying, synchronization, or incompatible placement may be introduced. Under that cost model, remove duplicate execution choices. Otherwise retain shared and recomputed alternatives, while still memoizing their common semantics.

**Counterexample:** two evaluations can each compute a temporary result in memory 1, while a shared retained realization holds that result across a much longer horizon or must span multiple time slices. Saving CPU does not dominate retained memory. Different media, concurrency, and consumer schedules also require comparison.

### E8. Condition on a separator, then optimize the remainder

The survey's “enumerate shared nodes, then DP the rest” is exact when the separator fixes **all** coupling: producer choices, consumer bindings, sizes/guarantees, materialization, windows/retention, shared raw charges, and relevant resource interfaces. Given that assignment, solve conditionally independent residual subproblems and compose their resource profiles.

Keeping only a shared node's presence bit is insufficient. Use variable elimination or tree-decomposition DP with full separator states. Complexity depends on domain sizes and induced width of the entire interaction graph, not simply the number of shared nodes or queries. Global resource constraints can enlarge the interface. This is an optional second search engine after conservative branch-and-bound, not a prerequisite for the first implementation.

## Bounds and branch-and-bound

### Safe bounds before sophisticated costing

Start with bounds whose validity is easy to audit:

1. **Zero:** valid for the configured nonnegative scalar objective. Weak but safe; reject negative or undefined inputs rather than assuming positivity.
2. **Mandatory exclusive work:** charge a proven lower bound for operations that every completion must execute and cannot share or replace. Charge each distinct operation once. Set potentially shareable producers, adapters with unknown implementations, and unresolved materializations to zero.
3. **Independent relaxed subproblems:** drop selected coupling constraints, allow conflicting choices to coexist, and optimize the remaining relaxed domains. The relaxation must contain every legal completion. Only sum terms whose accounting prevents double-counting; a sum of standalone query optima generally does not meet that requirement.
4. **LP relaxation:** encode candidate/build/binding choices and valid cost/resource under-estimators, then relax integer decisions to continuous ones. The relaxation optimum is a bound on the encoded exact objective when every original feasible plan maps into the relaxed model. A surrogate cost with no one-sided relation supplies no certified bound.

If a “sharing saving” is used, it must upper-bound the **total workload** saving under all completions, include benefits to other consumers, and count overlapping savings conservatively. That proof is harder than using an explicit relaxation. Use workload-level bounds rather than subtracting a query-local sharing estimate from its standalone cost.

An admissible bound need not be consistent between parent and child states for branch-and-bound correctness. A weak or zero bound can mean exhaustive exploration. Prefer a cheaply evaluated bound unless stronger pruning pays for its own planning overhead.

### Incumbents and resource bounds

Construct an initial feasible plan using standalone choices, then try compatible sharing and local improvement. A heuristic is useful here: each validated plan is a legitimate upper bound, even if it is far from optimal. If standalone choices violate a global budget, they are not an incumbent; search for a feasible one and report none if no plan has yet been found.

For cluster constraints, derive lower bounds on peak memory, disk, and cores using the same accounting contracts as the final model. Drop positive terms or relax dependencies when safe; do not add mutually exclusive configurations. A latency bound can come from an unavoidable dependency path with lower-bounded operator costs, not from summing work that can run in parallel.

For a requested Pareto front, an incumbent front may prune a branch only when every completion is dominated under the final strict-dominance/equality policy. A componentwise lower-bound vector helps: a feasible incumbent strictly dominating that bound dominates all completions under monotone aggregation. Equality is insufficient if equal-vector plan identities must be returned. A bound gap on one scalar objective does not certify an entire Pareto front.

### Numerical comparisons and ties

Current scalar ranking uses relative floating-point tolerance and retains an earlier candidate on a tie. Honor the tie contract above; do not let a different summation order create artificial dominance. Lower-bound arithmetic must round conservatively downward, or carry a justified error interval and prune only when its lower endpoint is strictly worse than the incumbent's upper endpoint, allowing for the ranking tolerance. An arbitrary epsilon is not a proof of numerical safety. Near ties and unknown bounds stay in the search. Stable ranks suffice for exact ties under a total ordering; the legacy tolerance case requires the additional compatibility handling described above.

## Composition under the three cost models

The implementation's `ResourceUsage` is the authority; cost model numbers are not evidence of additivity. In particular, model 1 can include a shared raw-retention disk charge and model 2 explicitly prices peaks.

| Model | Final choice | State needed for safe composition |
| --- | --- | --- |
| 1: `UsageOverTime` | Price CPU, scans, average memory, and configured disk use. | Charge each physical producer and execution frequency once; preserve per-source raw-retention horizons and other nonadditive accounting. Scalar component optima require proven separability, including the active prices. |
| 2: `ProvisionedCapacity` | Price peak memory, peak cores, and disk. | Keep peak/resource alternatives. Follow the current model's overlap assumptions; summing standalone price is not generally valid. |
| 3: `ClusterCapacity` | Reject memory/disk/core violations; choose lowest latency, least memory, or a Pareto front. | Carry constrained dimensions including **cores**, objective tie dimensions, and any boundary source/physical properties needed by composition. A front only over memory, disk, and latency can discard the sole core-feasible plan. |

A vector is sufficient only with a proved monotone composition rule for that interface. Under the current conservative all-evaluations-overlap memory model, some memory terms add. Disk retention combines by maximum per source; other retained state terms add. Worst-query latency combines by maximum for independent query groups, but changing a shared producer changes consumer paths. If a later model uses actual scheduling, an aggregate peak number may no longer suffice; retain a lifetime profile or disable that reduction.

For truly independent components with additive resource coordinates, combine their frontiers by resource DP (or exact enumeration) and discard dominated combined states. Use the same boundary signature during dominance comparisons. Exact continuous/large-integer frontiers may be large: rounding to buckets or capping a frontier is heuristic unless it has a separately proved approximation guarantee. Peak cores can be a feasibility dimension even when the displayed final front is over memory, disk, and latency only.

## Heuristics and what they give up

These are useful strategies, but their role must be explicit:

| Strategy | Safe use in an exact search | Approximate use that loses completeness |
| --- | --- | --- |
| Best standalone / greedy MQO / local improvement | Produce and validate incumbents. | Return the heuristic winner without finishing or certifying the remaining search. |
| Estimated savings, learned cost, query “difficulty” | Choose variable and alternative order; keep every unresolved branch. | Prune on a predicted saving or a learned lower bound without a validity certificate. |
| Per-query top-k, beam search, capped memo/front | Retain all alternatives externally and revisit them, or use only for ordering. | Drop all but k alternatives, potentially losing a donor producer or resource tradeoff. |
| Split by metric, observed sharing, or connected components of current winners | Schedule exploration in groups. | Assume those groups cannot interact under unexamined alternatives. |
| Always share equal expressions; always use the cheapest local materialization | Apply only after E6/E7 conditions are proved. | Force those choices on CPU-only intuition while ignoring retention and resource effects. |
| Epsilon-dominance, resource buckets, coarser sketch/window parameters | Order candidates or supply a known relaxation with one-sided bounds. | Remove near-equivalent states or configurations; exactness then holds only for a deliberately restricted domain, if at all. |
| Time, candidate, or memory budget | Pause with the full unresolved frontier represented; resume later. | Discard unresolved branches and claim completion, or report a gap that ignores dropped work. |

Greedy benefit does not automatically have diminishing returns here: resizing for additional consumers, overlapping reuse choices, and raw-input alternatives can create complements and conflicts. Do not quote a `1 - 1/e` approximation factor without proving the necessary objective and constraint structure for this model. Randomized search must record its seed; reproducibility is not optimality.

## Search procedure and stopping contract

The initial implementation uses depth-first branch-and-bound with a memo and lazy candidate construction. Best-first search can be added later, but keeping every frontier state in RAM can recreate the original memory problem. Depth-first search must retain the bounds of pending siblings and not silently drop work under memory pressure.

```text
freeze workload, rewrite/parameter inventory, deployment inputs, and evidence
build conservative interaction analysis and candidate groups lazily
validate heuristic completions to seed an incumbent (or no incumbent yet)
search(partial state):
    reuse a memo result only under an equivalent full interface
    reject permanently invalid branches (E2)
    bound all legal completions with a conservative relaxation
    reject branches certified infeasible or unable to improve (E3/E4)
    if complete:
        run the authoritative validity, resource, and ranking checks
        update the incumbent or requested Pareto front
    otherwise:
        choose an unresolved decision; enumerate all legal alternatives lazily
        explore promising alternatives first; record bounds for pending siblings
        stop or suspend only with the unresolved work represented in the report
```

Completeness must hold across all stages. A streaming Stage 3 is insufficient if Stage 1 first constructs a whole Cartesian product or Stage 2 first constructs every materialization. Share unchanged IR through immutable references; instantiate full physical plans only when needed. Diagnostic candidate dumps are an opt-in stream with their own storage limits, not mandatory retained search state.

Do not enumerate a full product to calculate a search percentage. After duplicate elimination and context-dependent rewriting, its size may be unknown. Report concrete counts and bounds rather than a fabricated fraction.

For scalar minimization report:

- the fixed rule/parameter/model/evidence versions and whether the candidate universe was restricted;
- feasible incumbent and its objective `U`, or “no feasible plan found”;
- a global lower bound `L`, or “bound unavailable”; when an incumbent exists, `L` must cover it and every unresolved branch, so use `min(U, all unresolved bounds)`;
- additive gap `U - L`; relative gap `(U - L) / abs(U)` when `U != 0`; an explicit zero-objective convention rather than division by zero;
- status: optimum certified, stopped with incumbent, stopped without incumbent, or infeasibility proved;
- stop reason, time/peak memory, generated/expanded/pruned counts, rule-specific certificates, memo hits, and remaining work;
- whether a heuristic **discarded** states, as distinct from merely ordering them. Include those states in a valid bound or mark the gap unavailable.

A budget can end with an optimality certificate if all remaining bounds rule out improvement; exhaustion is not required. Conversely, “no feasible plan found” after a budget is not proof of infeasibility. A Pareto request reports front completeness separately from the status of any selected representative. Gap claims are with respect to the frozen planner model and `F`, not real runtime or a larger omitted rewrite space.

## MILP as a relaxation and an alternative search

A facility-selection-shaped model is a useful starting point: choose one complete query alternative per query, activate required producer configurations once, and bind every consumer to a compatible producer or recomputation alternative. Include adapters and consumer-specific readout costs; require the relevant provider whenever its consumer is selected; exclude cycles and incompatible bindings. Producer configurations must encode family, size, window, phase, and retention, not merely “a summary exists.”

For a finite configuration inventory, binary choices can encode exact costs for those configurations without approximating the cost curve. The formulation is exact relative to `F` only if every legal generated plan is representable and every accepted solver solution is legal. Prove those two mappings on the oracle workloads. A compact model containing only some sharing patterns solves a different, restricted problem.

Hard constraints must include memory, disk, cores, and per-query accuracy/latency requirements where configured. For example, one raw-source retention variable can represent the maximum selected horizon using constraints for every active reader and a nonnegative disk price; additional constraints or a canonical minimum assignment are needed when the price is zero and the horizon affects feasibility/reporting. Critical-path latency and peak memory need explicit encodings consistent with the current model, not a sum substituted for them. Reprice and validate extracted plans through the existing selection path.

Relaxing a faithful formulation supplies lower bounds for E3. An integer solver provides incumbents and bounds for its model, subject to numerical tolerances. Piecewise approximations, continuous sketch sizing, and unsupported nonlinear costs require one-sided error bounds before their solver gap certifies anything about the original objective. A final original-model reprice validates the incumbent; it does not turn an invalid surrogate lower bound into a certificate.

Prototype MILP beside the separator/branch-and-bound engine on small exact instances, and compare objective, feasibility, bound strength, planning time, and memory. Do not make a solver dependency mandatory before the simpler exact reductions have measured benefits.

## Validation and implementation order

### Acceptance criteria

Use an independently retained exhaustive enumerator as the small-instance oracle. Freeze its candidate universe and compare optimum objective, feasibility, and the full requested Pareto vector set. Compare winner IDs only under the explicit stable tie contract. Search-node counts and memory are measurements, not equivalence evidence.

Each implemented pruning rule must have:

1. a statement of its preconditions and a proof or replacement witness;
2. an adversarial case where its preconditions do not hold, plus a positive case where the rule prunes;
3. exhaustive equivalence tests with the rule alone and in combination;
4. counters showing its actual planning-time and memory benefit;
5. a safe fallback when its assumptions are unknown.

### Required adversarial workloads

| Fixture | Property it tests |
| --- | --- |
| Donor producer costs 10, private work 6, standalone alternative 5; another consumer costs 100 or reads producer for 1. | Preserves donor-dependent reuse when evaluating candidate replacements. |
| Unshared choices `(cost 1, memory 10)` and `(cost 2, memory 1)`, memory budget 2. | Cheapest-local pruning must retain the feasible tradeoff. |
| Two disconnected computation groups each choose cheap/high-memory versus dear/low-memory. | Global capacity requires frontier combination, not independent scalar winners. |
| Equal displayed memory/disk/latency, different core requirements, one over the core limit. | Cores remain a partial-state feasibility dimension. |
| Same raw source with horizons 5 and 10; a rewrite exposes another compatible reader. | Per-source maximum is charged once; interaction analysis covers rewrite closure. |
| A shared producer meets a stricter reader only after resizing; an unsupported provisional form has a supported replacement. | Provisional validity failures are deferred; completed output accuracy is rechecked. |
| Same expression but different recurrence, lifetime, or physical property; sharing increases retained state. | Semantic equality alone cannot force a physical shared realization. |
| Three consumers, partial sharing subsets, overlapping rollup/capability/pane opportunities. | Pairwise savings and consumer counts do not stand in for full boundary state. |
| Structurally similar DAGs with different predicates, weights, unique keys, or guarantees; injected hash collisions. | Memo equality and CSE legality are checked beyond hashes. |
| Multiple traversal orders, equal costs, zero costs, near floating-point ties, and unknown model terms. | Stable ranking, conservative bounds, and no pruning from undefined estimates. |
| Stops before the first feasible plan, after an incumbent, and while pending branches have lower bounds below it. | Honest completion/infeasibility status and global bound accounting. |
| Exact/approximate scalar search and full Pareto reporting with the same incumbent. | A scalar certificate is not mistaken for complete Pareto enumeration. |

Generate small workloads with mixed recurrence, parameter sizes, accuracy evidence, capacities, and sharing patterns; for every visited branch check `LB <= oracle best completion` when a feasible completion exists. Also compare reported resource lower bounds with oracle completions and validate replacement witnesses. Test that changing a registered sharing rule or an evidence snapshot invalidates affected caches. For bounded search, increasing the expansion budget must never worsen the best objective in a deterministic resumed run; certificates must remain valid at every stop.

### Implementation sequence

| Step | Deliverable and guarantee | Measurement |
| --- | --- | --- |
| 1 | Lazy generation and streaming scalar/Pareto selection, unchanged feasible universe, explicit legacy-compatible or total-order tie handling, budget statuses. | Time to first feasible plan, total time, retained candidates, peak memory. |
| 2 | Duplicate-state memoization, permanent-invalidity checks, and initial zero/exclusive-work bounds. | Rule-specific pruning, bound cost, cache hits, oracle equality. |
| 3 | Conservative interaction analysis; separable components or exact component-frontier composition. | Component/separator sizes, frontier sizes, capacity cases, analysis cost. |
| 4 | Validated heuristic incumbents and stronger relaxation bounds; all heuristics used for ordering first. | Bound versus incumbent trajectory, time to certificate, expansion count. |
| 5 | Conditional local DP and sharing dominance only for proven interfaces; optional separator DP or MILP prototype. | Full oracle agreement, interface sizes, time/memory versus simpler search. |
| 6 | Explicit approximate mode with beam/front caps or restricted domains, only if exact mode remains too expensive. | Optimality gap against the oracle, available certificates, reproducible budget/seed. |

Run the planner-layering Examples 1–4, ASAPQuery's 30 mixed workloads, and the Kubernetes recording-rule workload. Report both exhaustive small-instance quality and large-instance scaling, including timeouts, missing coverage, and no-feasible-plan outcomes. “Tens of queries in seconds under a few GB” is a benchmark target; a giant interaction component or resource frontier may defeat it. Distinguish candidate-generation coverage, pruning speed, cost-model accuracy, and runtime correctness rather than attributing all four to search quality.

## References

1. [Moerkotte and Neumann, VLDB 2006: optimal bushy join trees without cross products](https://www.vldb.org/conf/2006/p930-moerkotte.pdf). Connected-subgraph enumeration within a defined join space; it does not justify deleting cross-query sharing choices.
2. [Graefe, 1995: The Cascades Framework for Query Optimization](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf). Memoization, properties, and guided search.
3. [Roy, Seshadri, Sudarshan, and Bhobe, SIGMOD 2000: Efficient and Extensible Algorithms for Multi Query Optimization](https://arxiv.org/abs/cs/9910021). Practical cost-based MQO heuristics, including greedy materialization.
4. [The #671 survey and view-selection references](https://github.com/ProjectASAP/ASAPPlanner/issues/671#issuecomment-6137638876). Background and candidate directions, not proofs of this proposal's pruning rules.
5. [Trummer and Koch: Solving the Join Ordering Problem via Mixed Integer Linear Programming](https://arxiv.org/abs/1511.02071). A finite join-order formulation with approximated costs; distinguish solver optimality from original-model optimality.
6. [PostgreSQL documentation: genetic query optimization](https://www.postgresql.org/docs/17/runtime-config-query.html#RUNTIME-CONFIG-QUERY-GEQO). An explicit example of trading exhaustive search for planning time.
7. [Selinger et al., SIGMOD 1979: Access Path Selection in a Relational Database Management System](https://research.ibm.com/publications/access-path-selection-in-a-relational-database-management-system). System R's cost-based access-path and join planning.
8. [Moerkotte and Neumann, SIGMOD 2008: Dynamic Programming Strikes Back](https://15721.courses.cs.cmu.edu/spring2020/papers/20-optimizer2/p539-moerkotte.pdf). Hypergraph-based join enumeration with DPhyp.
9. [Apache DataFusion: Query Optimizer](https://datafusion.apache.org/library-user-guide/query-optimizer.html). Logical rule passes and optimizer customization.
10. [Harinarayan, Rajaraman, and Ullman, SIGMOD 1996: Implementing Data Cubes Efficiently](https://dsf.berkeley.edu/cs286/papers/datacube-sigmod1996.pdf). Materialized-view selection over a data-cube lattice.
11. [CockroachDB optimizer package documentation: Memo](https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/opt/doc.go#memo). Defines memo groups and illustrates equivalent join expressions in the same group.
