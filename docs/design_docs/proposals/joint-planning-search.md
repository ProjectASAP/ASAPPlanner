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
- [Memo-group search](#memo-group-search)
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

## Memo-group search

The rules above prune a search that still builds and prices whole-workload candidates, one at a time, at about a millisecond each: RangeHeavy's 24-hour quantiles component checks 153,086 of them in 92 s, and whole workloads link most queries over one source into one component (shared scans, SQL windows as time ranges, sketch rollup). The memo-group search replaces whole-candidate enumeration with a Cascades-style memo over the workload's logical sub-expressions. The stages keep their semantics and code boundaries; what changes is the unit they exchange.

### Components

| Component | What it is |
| --- | --- |
| Memo group | One distinct logical sub-expression of the workload: the logical DAG hash-consed, so identical expressions of several queries are one group. Identical-expression sharing is group identity. |
| Implementation candidate | One way to implement one group: exact, exact accumulator, a summary family with its size parameters, tumbling panes, or a read of another group (newest panes of a shared chain, a rollup regrouping, a top-k ranking). It references child groups, not child nodes. A fused computation (e.g. a heap that absorbs a sum and a top-k) is one candidate that implements several groups; it records which. |
| Materialization | `NotMaterialized`, `MatAtQuery`, at ingestion, or kept across evaluations: chosen once per implementation candidate of a group, however many parents read it. |
| Shared group | A group read by two or more parents. Its choice (implementation, size, materialization, pane grid) couples its parents. |
| Coupling site | A choice several groups' costs depend on: a shared group's size (sized for its strictest reader), a shared pane chain's grid, longest window and size, a metric's scan label set, a source's raw-data horizon. |
| Local price | One implementation candidate's usage, priced by Stage 3's per-node pricing given its context (its readers' requirements, its coupling sites' thresholds): CPU operations, scanned bytes, memory byte-seconds, peak memory, disk, each reader's latency. Cached per (group, candidate, context). |
| Frontier | Per group and subproblem, the candidates no other dominates in usage: the cheapest under cost model 1, a Pareto front over (cost, peak memory, peak cores, disk, worst latency) under cost models 2 and 3. |

### How the stages and the memo work together

Today the stages run one after another over the whole workload, and each hands the next every whole-workload candidate; the candidate count is the product of every query's choices:

```text
Stage 0  frontend            -> pre-ASAP logical DAG
Stage 1  Pass 1              -> per target, whole sub-DAG candidates           (no cost)
         Pass 2              -> one whole-workload candidate per sharing subset (no cost)
Stage 2  materialization     -> every candidate x every materialization
Stage 3  selection           -> every physical candidate built, checked, priced (~1 ms each)
Stage 4  executor
```

The memo keeps each stage's semantics and code, and changes what they exchange: groups and implementation candidates instead of whole plans. Each stage runs per group, and only the choices that couple groups are made jointly:

```text
                 group A                 group B                 group C (shares nothing)
Stage 1 Pass 1   A's implementations     B's implementations     C's implementations
Stage 1 Pass 2   transformations add implementations that read another group (capability, panes, rollup, top-k)
Stage 2          materialization per implementation, once per group however many parents read it
Stage 3          local price per (group, implementation, context), cached; frontier per group
------------------------- coupling sites of the cluster {A, B} -------------------------
                 thresholds enumerated with bounds: a shared group's size (strictest reader),
                 a pane chain's grid and longest window, a metric's scan labels, a source's raw horizon;
                 dynamic programming below them; C is solved on its own
-------------------------------------------------------------------------------------------
Selected plan    composed once, then Stage 3's checks and the executor's compile check
```

Example: two queries over one metric.

```promql
Q1: quantile_over_time(0.9, data[5m])
Q2: topk(5, sum by (job) (sum_over_time(data[5m])))
```

Hash-consing their logical DAGs gives six groups; `G2` is one group read by both queries:

```text
G1 = Scan(data)
G2 = TimeRange(5m) over G1          <- read by Q1 and Q2
G3 = quantile_over_time(0.9) over G2    (Q1's root)
G4 = sum_over_time over G2
G5 = sum by (job) over G4
G6 = topk(5) over G5                    (Q2's root)
```

Pass 1 and the Pass 2 transformations give each group the implementations of that node alone, referencing child groups:

```text
G3: exact quantile (reads G2) | KLL(k) (reads G2) | DDSketch(alpha) (reads G2)
    | newest-n panes of a shared chain (reads another group's pane chain)
G4: exact sum accumulator (reads G2)
G5: exact group-by sum (reads G4)
G6: exact top-k (reads G5)
    | Count-Min heap (reads G2)     <- fused: one implementation of G4, G5 and G6
G1, G2: each reader computes its own (NotMaterialized) | built once per evaluation (MatAtQuery)
```

Before, the same `Scan -> TimeRange(5m)` was repeated inside every per-target candidate, and Pass 2 had to discover it again; candidates multiplied across targets. In the memo, `G2` appears once, `G3` has a handful of implementations, and the search is bottom-up: `G2`'s choice is a coupling choice of the cluster `{Q1, Q2}`, and every other group keeps its frontier given it.

Where each part of the stage design lives in the memo:

| Stage design | In the memo |
| --- | --- |
| Pass 1 computation candidates | implementation candidates per group, referencing child groups |
| Identical-expression rule | group identity (hash-consing) |
| Summary-capability rule | an implementation serving several reader groups; its size is a coupling site (strictest reader) |
| Window-composition rule | pane implementations in a group; a shared chain's grid and longest window are a coupling site; a reader reads its newest panes |
| Rollup and exact top-k reuse | an implementation of a coarse group (or a top-k) that reads a finer group (or a ranking) |
| Stage 2 materialization | chosen per implementation, once per group |
| Stage 3 pricing and checks | local price per (group, implementation, context), cached; the full plan is checked once |
| Raw retention, cost models 2 and 3, measured table | a per-source horizon site; frontiers; a calibration factor fitted once and frozen |

### Stage interfaces

- **Pass 1** emits, per group, its implementation candidates (exact, exact accumulators, each summary family and size, pane variants), each referencing child groups. It no longer emits a whole sub-DAG per target.
- **Pass 2 rules** add implementation candidates to groups:
  - summary capability: one build serving several reader groups, sized for the strictest reader;
  - window composition: pane implementations, a newest-n read of another group's chain, and the gcd grid;
  - rollup: a coarse group implemented by regrouping a finer group;
  - exact top-k reuse: a top-k implemented from another group's ranking.
- **Stage 2** chooses each candidate's materialization; a group with several parents chooses once.
- **Stage 3** prices per group with the cache. The executor's compile check and the full physical DAG are built only for the selection and the runner-ups a report asks for.

### Algorithm

1. **Hash-cons** the workload's logical DAG into groups.
2. **Generate** each group's implementation candidates (Pass 1, then the Pass 2 transformations), each with its child groups and its coupling sites.
3. **Clusters.** Groups linked by a shared group or a coupling site form a cluster; clusters are solved separately.
4. **Enumerate the coupling choices of a cluster, with bounds.** A shared group's size depends only on its strictest reader, so instead of reader subsets the search enumerates the strictness threshold (one value per distinct reader requirement, the per-parameter maximum when a size has several parameters); a reader no stricter than the threshold may read it, and does iff that is cheapest for it. Pane chains add the grid (the closure of the readers' widths under gcd) and the longest window. A metric's scans enumerate the label sets of their readers. A source's raw retention enumerates the horizon threshold among its raw-reading readers. Branch and bound prunes threshold combinations whose lower bound exceeds the incumbent.
5. **Dynamic programming** over the rest of the group DAG, given the coupling choices: each group keeps its frontier of candidates (cheapest, or Pareto under cost models 2 and 3), computed bottom-up from its children's frontiers with the cached local prices.
6. **Combine** the clusters' frontiers; peaks and capacity limits (cost models 2 and 3) are checked when frontiers combine. Latency and repeat-interval checks are checked when a reader attaches to a shared group, at that group's threshold.
7. **Build late.** Compose the selected plan once, run Stage 3's checks and the executor's compile check on it, and report it.

### Coupled charges

- **Raw data on disk**: a per-source coupling site whose size is the longest horizon among the readers that scan raw data, enumerated as a threshold (step 4).
- **Cost models 2 and 3**: frontiers instead of minima (step 5), peaks and limits at combination (step 6).
- **A measured cost table**: the calibration factor is fitted once, before the search, over every candidate node the generation produced (before pruning), and frozen; every group is priced with it.

### Exactness

The search is exact when every interaction between groups is a shared group or a coupling site, and each coupling site's cost depends only on its threshold. When a cluster has a construct no threshold captures, that cluster is searched exhaustively over its frontiers and the report names it; no cap is applied. Each rule is checked against exhaustive enumeration on small workloads (`joint_search_equivalence`), including raw-retention coupling, a cost model 2 peak, a cost model 3 capacity limit and a partial measured table.

### Code layout

`crates/plan-selection/src/pass/memo/`, behind `Stage1ToStage3Search::memo_groups` until it matches the current search on every equivalence test:

```rust
/// One hash-consed logical sub-expression.
pub(crate) struct MemoGroup { pub node: Rc<OperatorNode>, pub parents: Vec<GroupId>, pub queries: Vec<usize> }
/// One implementation of one group (or of several, for a fused computation).
pub(crate) struct ImplementationCandidate { pub implements: Vec<GroupId>, pub node: Rc<OperatorNode>, pub children: Vec<GroupId>, pub coupling_sites: Vec<CouplingSite> }
/// A choice several groups' costs depend on, keyed so that equal keys are one site.
pub(crate) struct CouplingSite { pub key: u64, pub kind: CouplingKind }
pub(crate) enum CouplingKind { SharedGroupSize, PaneChain, ScanLabels, RawHorizon }
/// The cached local price of a candidate in a context (thresholds of its sites).
pub(crate) fn local_price(group: GroupId, candidate: &ImplementationCandidate, context: &Context, cache: &mut PriceCache) -> Result<LocalUsage, SelectionError>;
/// The frontier of a group given its children's frontiers and a context.
pub(crate) fn frontier(group: GroupId, context: &Context, memo: &Memo, cache: &mut PriceCache) -> Frontier;
/// The cluster's selection: thresholds enumerated with bounds, the DP below them.
pub(crate) fn solve_cluster(cluster: &Cluster, memo: &Memo, cache: &mut PriceCache) -> Result<ClusterSelection, SelectionError>;
```

## Future work

- Stronger bounds from LP relaxations; separator/tree-decomposition DP or MILP as alternative search engines.
- Greedy incumbents and learned branch ordering to accelerate exact search.
- Explicit approximate modes such as beam search, top-k candidates, capped resource frontiers, and coarser parameter domains. These may lose the optimum and must be labeled accordingly.
- Incremental replanning and broader candidate/rewrite coverage.
