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
- [Memo-group search](#memo-group-search): how the stages and the memo work together, and an end-to-end walk-through with the sharing strategies
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
| MILP | One mixed-integer linear program over the memo whose variables are the groups' candidate choices, their materializations and the coupling sites' thresholds, with Stage 3's prices as coefficients. It replaces enumerating whole-workload candidates. |

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
Stage 3          local price per (group, implementation, context), cached: the MILP's coefficients
------------------------- coupling sites of the cluster {A, B} -------------------------
                 discrete threshold variables: a shared group's size (strictest reader),
                 a pane chain's grid and longest window, a metric's scan labels, a source's raw horizon;
                 one MILP chooses them together with every group's candidate; C's choice is independent
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

Before, the same `Scan -> TimeRange(5m)` was repeated inside every per-target candidate, and Pass 2 had to discover it again; candidates multiplied across targets. In the memo, `G2` appears once, `G3` has a handful of implementations, and the MILP has one variable per (group, implementation): `G2`'s materialization is a coupling choice that both queries' costs depend on.

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
| Raw retention, cost models 2 and 3, measured table | a per-source horizon threshold; linear peak and capacity constraints; a calibration factor fitted once and frozen |

### End to end: the memo with the sharing strategies

This walks one workload through the memo, showing what each sharing strategy (the identical-expression, summary-capability and window-composition rules, rollup and exact top-k reuse) contributes and where each decision is made. All queries read `data`, a gauge.

```promql
Q1: quantile_over_time(0.9, data[5m])                       every 10 s, relative error 0.01
Q2: quantile_over_time(0.5, data[1m])                       every 10 s, relative error 0.05
Q3: topk(5, sum by (job) (sum_over_time(data[5m])))         every 60 s
Q4: sum by (job, instance) (sum_over_time(data[5m]))        every 60 s
```

**1. Hash-cons the logical DAGs (identical-expression rule).** Each distinct sub-expression is one group. `Scan(data)` and `TimeRange(5m)` appear once, so every reader of them reads the same group; the identical-expression rule needs no separate pass.

```text
G1 = Scan(data)                                  read by all four queries
G2 = TimeRange(5m) over G1                       read by Q1, Q3, Q4
G3 = TimeRange(1m) over G1                       read by Q2
G4 = quantile_over_time(0.9) over G2             Q1
G5 = quantile_over_time(0.5) over G3             Q2
G6 = sum_over_time over G2                       read by Q3, Q4
G7 = sum by (job, instance) over G6              Q4
G8 = sum by (job) over G6                        read by Q3
G9 = topk(5) over G8                             Q3
```

**2. Pass 1: each group's own implementations.** Pass 1 gives every group the implementations of its node alone: `G4` and `G5` get exact, KLL and DDSketch, each whole-window or in tumbling panes; `G6` to `G9` get exact operators; `G9` also gets a fused Count-Min heap that implements `G6`, `G8` and `G9` at once.

**3. Pass 2: the sharing strategies add implementations that read other groups.**

| Strategy | Implementation it adds | Coupling site it creates |
| --- | --- | --- |
| Summary capability | `G4` and `G5` may read one DDSketch build over `data`, sized for the stricter reader (`G4`, error 0.01) | the shared build's size |
| Window composition | that build as one chain of 10 s panes over 5 m; `G5` reads its newest 6 panes, `G4` all 30 | the chain's grid (10 s, the gcd of the readers' widths), its longest window (5 m) and its size |
| Rollup | `G8` (`by (job)`) implemented by regrouping `G7` (`by (job, instance)`) | none beyond `G7` being read twice |
| Exact top-k reuse | where another query keeps the same ranking, `G9` reads it | the ranking's group |
| (scan widening) | `G1`'s readers read one scan with the union of their labels | the metric's label set |

Nothing here builds a whole-workload candidate; each strategy only adds alternatives to a group.

**4. Stage 2: materialization per implementation.** For example, the shared pane chain may be kept at ingestion time or rebuilt per evaluation; `G2` may be computed by each reader or built once per evaluation. A group with several parents chooses once.

**5. Stage 3: local prices as coefficients.** Each implementation is priced once per consumer rate, materialization and threshold, and cached; these are the MILP's coefficients. Here all nine groups are coupled through `G1`, `G2` and the shared quantile build. A second metric's groups would share no variable with them, and the solver separates such blocks on its own.

**6. One MILP chooses everything.** Threshold enumeration gives each coupling site its discrete values, which become MILP variables:

```text
shared quantile build:  size threshold in {for G4's 0.01, for G5's 0.05, no shared build}
  at 0.01: G5 may read it (0.05 >= 0.01) and does if that is cheaper for G5
  at 0.05: G4 cannot read it (0.01 is stricter), so G4 uses its own build
pane chain:             grid in {10 s}, longest window in {5 m, 1 m}
G2, G6:                 per reader or built once per evaluation
raw retention of data:  horizon threshold among the readers that scan raw data
```

The same MILP also chooses every other group's implementation: `G8` exact or rolled up from `G7`, `G9` exact or the fused heap. The threshold variables replace subsets of readers. The lazy-cut loop (below) then prices the solver's plan with Stage 3 and adds the held-memory term back until the two agree.

**7. Build late.** The selected choices are composed into one physical plan. Stage 3 checks it once (guarantees, latency against each repeat interval, capacity) and the executor's compile check runs on it alone. The runner-ups are built only if a report asks for them.

The result is the plan the exhaustive search would select. It is checked by `joint_search_equivalence` on workloads small enough for `--exhaustive`, one per strategy and per coupling site.

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
3. **Coefficients.** Price each candidate's private nodes with Stage 3's per-node pricing, per consumer rate and per materialization, and each coupling site's node at each of its thresholds. Stage 3 stays the only source of prices.
4. **Solve one MILP over the memo** (below): one choice per needed group, its materializations and the coupling sites' thresholds.
5. **Lazy cuts.** Compose the solver's plan, recompose its guarantees and price it with Stage 3. If Stage 3's price exceeds the solver's objective (the held-memory term), or a composed guarantee fails, add a cut that corrects or excludes that plan, and solve again. Stop when Stage 3's price equals the objective and every check passes. The gap must be 0 to report the plan as optimal.
6. **Build late.** Run the executor's compile check on the selected plan only, and report it.

### The solver: MILP over the memo

Threshold enumeration still turns each coupling site into discrete choices: a shared size set by one reader's requirement, a pane chain's grid and longest window, a scan's label set, a source's raw horizon. The MILP chooses among them instead of a hand-written search.

**Variables.**
- `x[g,c]`: group `g` implemented by candidate `c`. Groups a chosen candidate reads must be implemented; a fused candidate implements several.
- `m[s,o]`: materialization `o` of slot `s` (a candidate's build, or a shared group's node).
- `u[site,t]`: coupling site at threshold `t`; a reader may attach only to a threshold it may read.
- `r[k,g]`: consumer `k` (a query, or a materialized slot) reaches group `g` through unmaterialized nodes.
- `y[k,g,c] = r[k,g] ∧ x[g,c]`, linearized as `y ≥ r + x − 1`.

**Objective (cost model 1).** Σ rate(k) · per-consumer cost(g, c) · y, plus each materialized slot's own cost, each threshold's site cost, raw retention per source (`R_s ≥ bytes · query-time raw scan`), and pane retention (`keep ≥ span · read`). A slot built once per evaluation for readers on different grids takes the union rate through one indicator per distinct grid set.

**Constraints.** One candidate per needed group; down-closure (ingestion needs ingestion below); the reach propagation; attachment only to admitted thresholds; the combination rules (top-k reuse never with panes; rollup source rules); each query's latency (the sum of the work it reaches) within its bound and its repeat interval.

**Lazy terms.** Memory held while a query runs costs bytes × that query's latency, a product of two sums over its reach. The model omits it, which gives a lower bound, and step 5 adds it back through cuts on the incumbent. Composed accuracy guarantees depend on the chosen children; step 5 checks them on the incumbent and excludes failing combinations with no-good cuts.

**Cost models 2 and 3.** The same variables. Peaks and capacity limits become linear constraints over the per-node peak contributions, and each objective is as specified.

**Formulation.**

Sets:
- `Q`: queries; `G`: memo groups; `C(g)`: candidates of group `g`.
- `ch(g,c)`: child groups that candidate `(g,c)` reads; `impl(g,c)`: groups it implements (more than `{g}` for a fused candidate).
- `O = {nm, mq, ing, kept}`: materializations (not materialized, built once per evaluation, at ingestion, kept across evaluations).
- `S`: coupling sites; `T(s)`: thresholds of site `s`.
- `K = Q ∪ {(g,c,o) : o ≠ nm}`: consumers, the queries and the materialized slots.

Parameters, all priced by Stage 3, in cost per second unless noted:
- `rate(k)`: evaluations per second of consumer `k`.
- `w(g,c)`: cost of one computation of `(g,c)`'s own nodes.
- `a(g,c,o)`: standing cost of slot `(g,c,o)` (ingestion upkeep, kept state).
- `σ(s,t)`: cost of site `s` at threshold `t`.
- `may(s,t,g)`: whether group `g` may read site `s` at threshold `t`.
- `ℓ(g,c)`: latency of one computation, in ms.
- `B(q)`: the latency limit of query `q`, `min(max_latency_ms, repeat interval)`.
- `hz(n)`: raw horizon bytes of raw scan `n`; `span(q,h)`: retention that reader `q` needs from pane chain `h`; `β(h)`: cost per second of keeping one span unit of `h`; `p_disk`: disk price.

Variables:

```text
x[g,c]   ∈ {0,1}   group g is implemented by candidate c
m[g,c,o] ∈ {0,1}   candidate (g,c) is materialized as o
u[s,t]   ∈ {0,1}   coupling site s takes threshold t
r[k,g]   ∈ {0,1}   consumer k computes group g (reaches it through unmaterialized nodes)
y[k,g,c] ∈ {0,1}   r[k,g] ∧ x[g,c]
λ[q,g]   ∈ {0,1}   g is computed within q's evaluation (reached through nm or mq nodes)
R[s]     ≥ 0       raw horizon kept for source s
keep[h]  ≥ 0       retention kept for pane chain h
θ        ≥ 0       held memory (memory × latency), added by lazy cuts
```

Objective, cost model 1:

```text
min  Σ_k Σ_(g,c) rate(k) · w(g,c) · y[k,g,c]      computation, once per consumer that reaches it
   + Σ_(g,c,o) a(g,c,o) · m[g,c,o]                standing cost of materialized slots
   + Σ_(s,t)   σ(s,t) · u[s,t]                    coupling sites at their thresholds
   + p_disk · Σ_s R[s]  +  Σ_h β(h) · keep[h]     raw and pane retention
   + θ                                            held memory (lazy)
```

Constraints:

```text
(1) cover      Σ_{(h,c): g ∈ impl(h,c)} x[h,c] = need[g]            each needed group covered once
               need[root(q)] = 1;   need[g] ≥ x[p,c]  for g ∈ ch(p,c)
(2) materialize Σ_o m[g,c,o] = x[g,c]
               m[g,c,ing] ≤ Σ_c' m[g',c',ing] + rawsource(g')   for g' ∈ ch(g,c)   (ingestion needs ingestion below)
(3) reach      r[q,root(q)] = 1;   r[(g,c,o),g'] ≥ m[g,c,o]      for g' ∈ ch(g,c)
               r[k,g'] ≥ r[k,p] + x[p,c] − Σ_{o≠nm} m[p,c,o] − 1  for g' ∈ ch(p,c)
(4) product    y[k,g,c] ≥ r[k,g] + x[g,c] − 1
(5) sites      Σ_t u[s,t] ≤ 1;   x[g,c] ≤ Σ_{t: may(s,t,g)} u[s,t]  for each candidate (g,c) that reads site s
(6) retention  R[s] ≥ hz(n) · λ[q,n]   for each raw scan n of s and each q
               keep[h] ≥ span(q,h) · x[q's reader of h]
(7) latency    λ[q,root(q)] = 1;   λ[q,g'] ≥ λ[q,p] + x[p,c] − m[p,c,ing] − m[p,c,kept] − 1
               Σ_(g,c) ℓ(g,c) · (λ[q,g] + x[g,c] − 1)^+  + compaction(q)  ≤  B(q)
               (the product linearized as in (4))
(8) rules      combination limits, e.g. x[top-k reuse] + x[pane candidate of the same group] ≤ 1
```

Minimization keeps `r`, `y`, `λ`, `R` and `keep` at their smallest feasible values, so their lower-bound constraints make them exact.

Lazy cuts, added by the loop for an incumbent plan `P` (a set of chosen `(g,c)` and `(g,c,o)`):

```text
held memory     θ ≥ H(P) · ( Σ_{(g,c) ∈ P} x[g,c] − |P| + 1 )     H(P) = Stage 3's held-memory cost of P
accuracy        Σ_{(g,c) ∈ B} x[g,c] ≤ |B| − 1                     B = a combination whose composed guarantee fails
```

Each held-memory cut is binding only when every choice of `P` is made, so it never cuts off another plan, and its bound is exact for `P`.

Cost models 2 and 3 keep (1)–(8) and the cuts, and change the objective:
- **Cost model 2:** `min π_mem · M + π_cpu · N + π_disk · D`, with `M ≥ Σ peak contributions`, `N ≥` ingestion cores `+` concurrently evaluating queries, and `D ≥` retention.
- **Cost model 3:** no prices. Add `M ≤ cap_mem`, `D ≤ cap_disk` (and `N ≤ cap_cores`), then minimize the chosen objective: the worst latency `L ≥ L_q` for every `q`, or `M`.

**Why a MILP rather than a hand-written search.** The memo is small: the whole paper workloads have 74 groups and 284 candidates (W-RangeHeavy) and 158 groups and 584 candidates (W-RollupHeavy), each built in under 0.3 s. Cost model 1 is not additive per node:
- memory held while a query runs costs bytes × that query's latency;
- how often a node runs depends on the materialization choices above it.

A branch and bound with dynamic programming would need custom bounds for both, plus Pareto frontiers for cost models 2 and 3. In a MILP each of these is a standard exact linearization or a linear constraint, and the solver supplies the bounds, the search and an optimality certificate.

This is the multi-query optimization problem. The memo is its AND-OR DAG: a group is an equivalence node and a candidate is an operation node. Greedy multi-query optimizers (for example Roy et al., SIGMOD 2000) give up optimality for speed. Exact formulations are integer programs. Our shared sizes depend on the strictest reader, accuracy guarantees compose, and time enters through panes and retention, so we need the exact form.

**How each non-additive term is encoded.**

| Term (cost model 1) | Encoding | Exact |
| --- | --- | --- |
| How often a node runs: once per consumer whose path reaches it through unmaterialized nodes | reach indicators `r[k,g]` propagated through `x` and `m`; cost on `y = r ∧ x` | linear |
| A node built once per evaluation for readers on different grids | one indicator per distinct set of reader grids; cost at that set's union rate | linear |
| A shared size set by the strictest reader (capability builds, pane chains, rollup state) | one indicator per threshold; a reader attaches only to a threshold it may read | linear |
| A scan's label set | one indicator per label set | linear |
| Raw retention: the longest horizon among query-time raw scans of a source | `R_s ≥ bytes(n) · query-time(n)` | linear |
| Pane retention: the longest span among a chain's readers | `keep ≥ span(q) · read(q, chain)` | linear |
| Ingestion compaction: depends on the ingest worker count, a ceiling | integer workers `≥ work / capacity` | linear |
| Latency within the bound and the repeat interval | the sum of the reached work, over `y` | linear |
| Memory held while a query runs: bytes × latency | omitted from the model; added back by lazy cuts | lazy |
| Composed accuracy guarantees | checked on the incumbent; failing combinations excluded | lazy |
| A measured table's calibration factor | fitted once before the search | constant |

An exact linearization of the held-memory product needs a variable for each query and each pair of groups it reaches (about 3M on W-RollupHeavy with 8 users), so it is generated lazily instead.

**The lazy-cut loop.** The model without the lazy terms never overestimates a plan's cost, so its optimum is a lower bound on the true optimum. The loop:
1. Solve the model and take its plan `P`, with objective `c_milp`.
2. Compose `P` and price it with Stage 3, which includes held memory and the composed guarantees, giving `c_true`.
3. If `c_true = c_milp` and every check passes, `P` is optimal: its true cost equals a lower bound on every plan's true cost.
4. Otherwise add a cut and go to step 1:
   - a combination whose guarantee fails is excluded;
   - a combination whose cost was underestimated gets "choosing this combination costs at least `c_true`".

Each cut removes or corrects at least one of finitely many combinations, so the loop ends, and it ends at the optimum. For example:
- Round 1 picks `P1` at 0.40 per second, and Stage 3 prices it at 0.47. A cut makes `P1`'s combination cost at least 0.47.
- Round 2 picks `P2` at 0.45, and Stage 3 agrees. `P2` is optimal.

(These numbers are illustrative.) The cost of the loop is the number of rounds. Tight McCormick bounds on held memory (from the memo's byte and latency ranges) and several cuts per round keep it low; the report gives the rounds per workload.

**Reporting.** The result states the solver gap. A time limit that ends with a nonzero gap is reported with that gap, never as optimal.

**Validation.** The brute force over the memo is a second oracle. `joint_search_equivalence` checks the MILP, the brute force and `--exhaustive` against each other on small workloads under all three cost models: a peak case, a capacity case and a raw-horizon case.

### Coupled charges

- **Raw data on disk**: a per-source horizon threshold, linear in the MILP.
- **Cost models 2 and 3**: peaks and limits as linear constraints.
- **A measured cost table**: the calibration factor is fitted once, before the search, over every candidate node the generation produced (before pruning), and frozen; every coefficient is priced with it.

### Code layout

`crates/plan-selection/src/pass/subexpr/`: `groups.rs` (hash-consing, Pass 1), `capability.rs`, `windows.rs`, `scans.rs` (the Pass 2 transformations), `pricing.rs` (per-node prices in a consumer context), `search.rs` (the memo and the brute-force reference), `milp.rs` (the model, the lazy-cut loop). The search is chosen by `--search legacy|memo-brute-force|memo-milp`; the MILP becomes the default once it matches `--exhaustive` on every equivalence test.

## Future work

- Stronger LP relaxations and McCormick bounds for the held-memory term; tree-decomposition dynamic programming as an alternative engine for the MILP.
- Greedy incumbents and learned branch ordering to accelerate exact search.
- Explicit approximate modes such as beam search, top-k candidates, capped resource frontiers, and coarser parameter domains. These may lose the optimum and must be labeled accordingly.
- Incremental replanning and broader candidate/rewrite coverage.
