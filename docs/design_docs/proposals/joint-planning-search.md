# Joint planning search and exact pruning

Status: proposal; this document does not implement new pruning. Audience: planner designers and implementers.

ASAPPlanner searches per-query alternatives, sharing choices, and materializations to select a whole-workload plan. The design preserves the stages in [planner-layering.md](planner-layering.md) while reducing redundant search. It follows the [survey in #671](https://github.com/ProjectASAP/ASAPPlanner/issues/671#issuecomment-6137638876).

## Optimizer search survey

| Approach | How it searches | Guarantee |
| --- | --- | --- |
| [System R-style DP](https://research.ibm.com/publications/access-path-selection-in-a-relational-database-management-system) | Builds larger plans from smaller subproblems, retaining alternatives with useful physical properties. | Exact within its enumerated plan space when the state captures every property needed by a parent. |
| [Volcano/Cascades](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf) | Stores equivalent expressions in memo groups; explores alternatives on demand with cost limits. | Requires sufficient context in memo keys and valid pruning bounds. |
| [Multi-query optimization](https://arxiv.org/abs/cs/9910021) | Chooses which common computations to build, materialize, and reuse across queries. | Greedy sharing is practical but generally heuristic; consumers' choices are coupled. |
| [MILP](https://arxiv.org/abs/1511.02071) | Encodes plan choices and constraints for a solver. | Optimality applies to the encoded alternatives and objective, including any cost approximations. |
| [Bounded/randomized search](https://www.postgresql.org/docs/17/runtime-config-query.html#RUNTIME-CONFIG-QUERY-GEQO) | Limits exploration to control planning time. | Can miss the exhaustive optimum. |

For ASAPPlanner, retain equivalent search states, reject impossible branches, and prune only with a valid workload-level bound or replacement proof. Sharing prevents a simple “keep the cheapest plan per query” DP from being exact.

## Exactness contract

“Exact” means preserving the optimum of the existing finite candidate space under the configured cost, accuracy, and capability models. It does not guarantee the fastest measured runtime or cover alternatives the generator never offers. Freeze the workload, rule/parameter inventory, and model/evidence inputs for each search.

A memo group is standard optimizer terminology for logically equivalent expressions; see [Cascades, Section 2](https://15721.courses.cs.cmu.edu/spring2019/papers/22-optimizer1/graefe-ieee1995.pdf) and [CockroachDB's optimizer documentation](https://github.com/cockroachdb/cockroach/blob/master/pkg/sql/opt/doc.go#memo). Approximate alternatives must also carry their output-accuracy obligations.

A reusable search state must expose everything later choices can observe: source and expression semantics, schema, summary parameters and guarantees, consumers, windows, recurrence, retention, materialization, physical properties, and resource obligations. Unknown interactions remain connected; a matching hash alone never proves equivalent states.

## Exact pruning methods

Each method applies only when its stated conditions hold. Otherwise retain the alternative. These are correctness conditions, not promises of a particular pruning rate.

### 1. Eliminate duplicate states

Expand one representative when two states have the same observable interface, remaining choices, and cost/resource obligations. Verify equality after hashing and retain required provenance and tie information.

This is exact because both states have the same completions. Memoizing equivalent computations does not force their runtime results to be shared.

### 2. Reject permanently invalid branches

Reject a branch only when no remaining choice can satisfy semantics, capabilities, executor support, or output accuracy. Examples include an unrepairable type error or a fixed unsupported operator with no legal replacement.

Defer failures that another choice can repair: resizing can improve accuracy, changing summary family can supply evidence, and ingestion materialization can remove a query-time raw-data requirement. Recheck completed plans with the authoritative validator.

### 3. Prune using a workload lower bound

For a partial state `S`, require `LB(S) <= cost(P)` for every feasible completion `P`. With a validated incumbent of cost `U`, prune only when `LB(S) > U`, conservatively accounting for numeric error and the selection tie policy. A resource lower bound exceeding a hard limit also rejects the branch.

Start with zero for nonnegative objectives, then add proven unavoidable, nonshareable charges, counted once. Potentially shareable or replaceable work contributes zero until a stronger bound is proved. A partial plan's current cost, or a sum of standalone query costs, is not automatically a lower bound.

### 4. Remove contextually dominated alternatives

Discard B only if every completion of B can be replaced by a feasible completion of A with compatible outputs and reusable producers, no higher objective, and no worse constrained resources. A sufficient case is an identical boundary interface with a componentwise no-worse resource/cost vector under a monotone composition rule.

Keep useful resource tradeoffs and sharing capabilities. For example, a producer costing 10 plus private work costing 6 can let another query cost 1 instead of 100. Replacing that first query with a standalone plan costing 5 can lose the joint plan costing 17. Standalone price alone does not establish dominance.

### 5. Split independent components

Analyze possible interactions across **all** alternatives: identical computations, capability sharing, rollup, top-k reuse, windows, and shared resource charges. Include interactions exposed by later rewrites. If independence is unknown, keep the components together.

Choose each component's scalar optimum separately only when feasibility factors and the objective is additive. With global capacity limits, retain and combine resource frontiers instead. Include cores as well as memory, disk, and objective dimensions. Raw retention uses the maximum horizon per source, so source accounting can couple otherwise separate computations.

### 6. Resolve isolated materialization choices locally

Choose the cheapest local realization only after fixing its inputs, outputs, lifetimes, and required properties, and proving that it has no remaining cost or resource interaction. Otherwise retain nondominated realizations for that interface.

An unshared build is not automatically isolated: it can share a scan or affect ancestor working memory and raw retention. Under a memory limit of 2, alternatives `(cost 1, memory 10)` and `(cost 2, memory 1)` require keeping the second option.

### 7. Force sharing only when it dominates recomputation

First apply the existing [CSE legality checks](../../../crates/types/src/ir/cse.rs). Then eliminate recomputation only if the shared physical realization is no worse on every relevant cost, resource, and consumer property.

Equal expressions alone do not prove this: sharing can extend retention or introduce copying, synchronization, or scheduling costs. When dominance is unproved, retain both physical alternatives and memoize their common semantics.

## Search and validation

Generate candidates lazily and stream completed plans through the existing validation and ranking path. Apply the rules above as soon as their preconditions are known. Unknown bounds and numerical near-ties remain searchable. Preserve legacy selection order for tolerance-sensitive ties unless a new total-order policy is explicitly adopted; stable IDs alone do not reproduce a nontransitive tolerance comparison.

Exact search finishes when every remaining branch has been explored or certified unable to improve the result. A budget stop reports the best feasible plan found, unresolved work, and whether optimality is certified. No incumbent means “no feasible plan found,” not “infeasible.” Report a scalar gap only with a bound covering all unresolved branches; report Pareto-front completeness separately.

Validate each rule against exhaustive enumeration on small workloads, alone and in combination. Include donor-dependent sharing, global memory/core limits, overlapping reuse, per-source retention, repairable accuracy failures, and near ties. Verify bounds against the best exhaustive completion of each tested partial state. Measure planning time and peak memory on Examples 1–4, the mixed workloads, and Kubernetes recording rules without claiming a universal scaling guarantee.

## Future work

- Stronger bounds from LP relaxations; separator/tree-decomposition DP or MILP as alternative search engines.
- Greedy incumbents and learned branch ordering to accelerate exact search.
- Explicit approximate modes such as beam search, top-k candidates, capped resource frontiers, and coarser parameter domains. These may lose the optimum and must be labeled accordingly.
- Incremental replanning and broader candidate/rewrite coverage.
