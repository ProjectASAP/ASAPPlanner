# Joint planning search and exact pruning

Status: proposal; this document does not implement new pruning. Audience: planner designers and implementers.

ASAPPlanner searches per-query alternatives, sharing choices, and materializations to select a whole-workload plan. The design preserves the stages in [planner-layering.md](planner-layering.md) while reducing redundant search. It follows the [survey in #671](https://github.com/ProjectASAP/ASAPPlanner/issues/671#issuecomment-6137638876).

## Contents

- [Problem](#problem)
- [Goals and non-goals](#goals-and-non-goals)
- [Where the search space comes from](#where-the-search-space-comes-from)
- [Optimizer search survey](#optimizer-search-survey)
- [Exactness contract](#exactness-contract)
- [Design overview](#design-overview)
- [Exact pruning methods](#exact-pruning-methods)
- [Interfaces](#interfaces)
- [Worked example](#worked-example)
- [Search and validation](#search-and-validation)
- [Future work](#future-work)

## Problem

Planning a workload jointly lets queries share computation: one summary answers several queries, identical sub-expressions run once, and panes are reused across evaluations. Today Stage 1 to Stage 3 enumerate every whole-workload candidate, price each one, and keep all of them in memory.

That does not scale past a handful of queries. Planning a growing prefix of one 10-query workload of ASAPQuery's random mixed workloads:

| Queries | Time | Peak memory |
| ---: | ---: | ---: |
| 3 | 0.4 s | 0.14 GB |
| 4 | 5 s | 1.5 GB |
| 5 | 129 s | 24.7 GB |
| 6 | stopped at about 228 s | about 100 GB |

Each added query multiplies the work by roughly 20. None of those workloads, and not the Kubernetes recording-rule set, can be planned as a whole. Each query planned alone takes milliseconds, but then sharing across queries is lost.

## Goals and non-goals

**Goals**

- Joint planning of realistic workloads (tens of queries) within seconds and a few GB.
- The same selected plan as exhaustive search over the same candidate space (see [Exactness contract](#exactness-contract)).
- Bounded search that says what it did: when a budget stops the search, the result reports it and whether optimality is certified.

**Non-goals**

- Changing what the stages decide. Candidate generation, sharing rules, materializations, and the accuracy and cost checks stay as they are.
- Approximate search modes. They are future work and must be labeled as such.
- Incremental replanning.

## Where the search space comes from

| Stage | Choice | Grows as |
| --- | --- | --- |
| Stage 1, Pass 1 | One per-computation candidate per target | the product of the targets' candidate counts |
| Stage 1, Pass 2 | Sharing alternatives of each composed candidate: identical expressions, summary capability, window composition, rollup, top-k reuse | the subsets of the sharing opportunities |
| Stage 2 | One materialization per summary build: at ingestion, at query time, or at query time kept across evaluations | three per build |

Most combinations cannot be selected. Either a cheaper alternative replaces them in every completion, or their choices in one part of the workload do not interact with another part. The methods below remove those combinations without removing the optimum.

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

## Design overview

The search keeps the stages' semantics and changes only how their choices are explored.

**Components**

| Component | Role |
| --- | --- |
| Search state | A partial whole-workload plan: the choices made so far, plus everything later choices can observe (see [Exactness contract](#exactness-contract)). |
| Memo | Equivalent search states, grouped so that each is expanded once (method 1). |
| Interaction graph | Which computations can interact through sharing or shared resource charges. Its connected components are searched separately when that is exact (method 5). |
| Lower bound | A valid lower bound on the cost of every completion of a state (method 3). |
| Incumbent | The best validated complete plan so far, or a resource frontier when global limits apply. |
| Budget | Limits on states expanded or time, with a report of unresolved work when it stops the search. |

**Search loop**

1. Build the interaction graph from all alternatives, and split the workload into components where independence is proven (method 5).
2. For each component, generate states lazily, in the existing enumeration order.
3. For each new state, apply in order:
   - duplicate elimination (method 1);
   - permanent-invalidity rejection (method 2);
   - lower-bound pruning against the incumbent (method 3);
   - contextual dominance (method 4).
4. When a materialization or sharing choice becomes isolated, resolve it locally (methods 6 and 7) instead of branching on it.
5. Complete plans go through the existing validation and ranking path, and update the incumbent.
6. Combine the components' results: the sum of their optima when the objective is additive and feasibility factors, otherwise their resource frontiers.

Every pruning step requires its stated precondition. When the precondition is unknown, the alternative stays in the search.

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

## Interfaces

These are design-level interfaces; names in the implementation may differ.

**Inputs**
- The existing Stage 1 to Stage 3 inputs: workload, planning models, plan selection settings.
- A search budget in plan selection settings: limits on expanded states and on time, with a large default.
- An exhaustive mode, used by tests to compare results.

**Outputs**
- The selected plan, as today.
- A search report:
  - the components found;
  - per component: states generated, states pruned by each method, and plans validated;
  - whether the search completed;
  - if a budget stopped it: the best feasible plan found, the unresolved work, and whether optimality is certified.
- Developer tools can still request the full candidate lists; normal planning keeps only the incumbent.

## Worked example

A workload of three recurring queries:

```promql
q1: sum by (job) (rate(http_requests_total[5m]))
q2: topk(5, sum by (job) (rate(http_requests_total[5m])))
q3: quantile_over_time(0.99, latency_ms[5m])
```

- **Method 5.** q1 and q2 read the same source and can share the per-job rate sum (identical computation and top-k reuse), so they form one component. q3 reads another source. It has no sharing opportunity with q1 or q2, so under an additive objective with no global limit it is searched on its own. With a global memory or core limit, the two components keep resource frontiers and are combined at the end.
- **Method 1.** In the {q1, q2} component, the composed candidates that differ only in how the identical rate sum is numbered are one state.
- **Method 7.** Sharing the per-job rate sum is forced only if the shared realization is no worse on every relevant cost, resource and consumer property. Otherwise both the shared and the separate realization stay.
- **Method 6.** q3's per-series KLL panes have no interaction with the other component. Their materialization can be chosen locally once their inputs, lifetimes and raw-retention effect are fixed. If the choice changes raw retention for `latency_ms`, the nondominated realizations are kept instead.
- **Method 3.** Once a validated plan for {q1, q2} exists, a partial state whose unavoidable, nonshareable charges already exceed it is pruned.

The selected plan is the same as exhaustive search's. The number of states expanded drops from the product over all three queries to roughly the sum over the components.

## Search and validation

Generate candidates lazily and stream completed plans through the existing validation and ranking path. Apply the rules above as soon as their preconditions are known. Unknown bounds and numerical near-ties remain searchable. Preserve legacy selection order for tolerance-sensitive ties unless a new total-order policy is explicitly adopted; stable IDs alone do not reproduce a nontransitive tolerance comparison.

Exact search finishes when every remaining branch has been explored or certified unable to improve the result. A budget stop reports the best feasible plan found, unresolved work, and whether optimality is certified. No incumbent means “no feasible plan found,” not “infeasible.” Report a scalar gap only with a bound covering all unresolved branches; report Pareto-front completeness separately.

Validate each rule against exhaustive enumeration on small workloads, alone and in combination. Include donor-dependent sharing, global memory/core limits, overlapping reuse, per-source retention, repairable accuracy failures, and near ties. Verify bounds against the best exhaustive completion of each tested partial state. Measure planning time and peak memory on Examples 1–4, the mixed workloads, and Kubernetes recording rules without claiming a universal scaling guarantee.

## Future work

- Stronger bounds from LP relaxations; separator/tree-decomposition DP or MILP as alternative search engines.
- Greedy incumbents and learned branch ordering to accelerate exact search.
- Explicit approximate modes such as beam search, top-k candidates, capped resource frontiers, and coarser parameter domains. These may lose the optimum and must be labeled accordingly.
- Incremental replanning and broader candidate/rewrite coverage.
