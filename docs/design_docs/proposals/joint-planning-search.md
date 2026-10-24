# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal

**Scope:** Multi-query optimization across exact operators, ASAP primitives, window implementations, sharing, and materialization.

## 1. Motivation and Challenges

ASAP primitives create opportunities to replace expensive exact computations and share summaries, panes, rollups, scans, or retained raw data. They also enlarge the optimization search space. Enumerating complete workload candidates multiplies implementation, sharing, window, and materialization choices. Optimizing queries independently avoids that enumeration but can miss joint savings.

The challenge is cross-query dependence: a candidate's incremental cost changes when another candidate enables a shared producer, requires a larger summary, or extends retention. Selecting the cheapest candidate for each group independently can therefore lose the workload optimum.

This doc aims to address the problem of finding a minimum-cost physical execution DAG for a workload of queries under the deployment's cost model. The plan must produce every requested result, satisfy each query's accuracy and latency requirements, respect resource limits, and account for shared computation and retained state without duplicate cost charging. This requires jointly deciding which supported implementations to use, which computations to share, and when to materialize their outputs, while keeping ASAPPlanner's planning time and planning runtime memory reasonable. Optimality is relative to the supported finite candidate space and configured models. Developing new sketch/window algorithms and runtime deployment is outside the scope.

## 2. Traditional Memo Groups and Rule Expansion

A traditional memo, as used by Cascades-style optimizers, compactly represents equivalent query expressions. A **memo group** contains expressions that produce the same logical result under the query semantics. Each expression references child groups instead of copying complete input plans. Different parent expressions can therefore reuse the same search work.

### Logical rule expansion

Consider an inner join with a filter on `A`:

```sql
SELECT *
FROM A JOIN B ON A.id = B.id
WHERE A.x > 10;
```

Initially, the memo can contain:

```text
G1 = { Scan(A) }
G2 = { Scan(B) }
G3 = { Join(G1, G2, A.id = B.id) }
G5 = { Filter(A.x > 10, G3) }               query result
```

A filter-pushdown rule matches the filter above the inner join. Because the predicate depends only on `A` and pushing it down preserves semantics, it adds a filtered-input group and another expression to the existing result group:

```text
G4 = { Filter(A.x > 10, G1) }

G5 = {
    Filter(A.x > 10, G3),
    Join(G4, G2, A.id = B.id)
}
```

Both members of `G5` produce the same result. The rule expands the group rather than immediately replacing its existing expression. Newly introduced computations, such as the filtered `A`, can create new groups. Further rules can match newly inserted expressions; interning avoids repeating identical expressions. Exploration may be demand-driven and interleaved with costing rather than saturating every possible rewrite.

### Physical implementation expansion

Implementation rules turn logical expressions into physical execution candidates. For the join expression in `G5`, eligible candidates can include:

```text
HashJoin(G4, G2)
MergeJoin(G4, G2)
NestedLoopJoin(G4, G2)
```

All implement the same logical join result, but impose different input requirements and costs. A merge join may require inputs ordered by the join key; a hash join does not require that order. Physical expressions continue to reference child groups, whose implementations are chosen separately. The precise storage of logical and physical expressions varies by optimizer.

The optimizer typically searches and caches:

```text
Best(group, required_physical_properties)
```

For each physical candidate, it derives child requirements, optimizes the child groups under those requirements, and combines their costs with the candidate's own cost. The cheapest implementation for unordered output may differ from the cheapest one for ordered output; sorting or other property-enforcing operators may be needed. Rules generate candidates; costing and valid bounds guide exploration and selection.

Sharing a memo group shares optimization representation, not necessarily runtime computation. Explicit materialization/reuse decisions are still needed to execute a common producer once. This distinction becomes central to ASAPPlanner's cross-query optimization below.

## 3. ASAPPlanner Memo Groups and Rule Expansion

Given the benefits of Memo groups, ASAPPlanner should introduce the Memo group while representing summaries and reuse strategies.

In ASAPPlanner, we extend the definition of a **memo group**, whose output can be a query or intermediate relational result, or reusable intermediate state. A result group preserves input, filters, grouping, window semantics, and requested output. A state group additionally specifies its state family/interface and guarantees. **Implementation candidates** provide exact computation, accumulators, or summaries and reference input groups. Physical candidates add legal operators and materialization assignments. A **physical instance** is one enabled realization of a group, with its own selected candidate, materialization, consumer set, and lifetime. Several instances of the same group can coexist. A **shared producer** is an instance read by several consumers, not a separately selected catalog entry.

Unlike exact expressions in a traditional equivalence group, approximate candidates target the same requested result under explicit accuracy obligations. State candidates must satisfy their group's contract, and consumers can impose stricter compatibility requirements. One group identity neither forces shared execution nor limits the plan to one physical instance. A stage-level candidate remains a whole-workload DAG; local group candidates compactly represent its component choices.

The following example traces how the proposed memo expands:

```text
Q1: quantile_over_time(0.95, X[15m]), every 1m
Q2: quantile_over_time(0.99, X[15m]), every 1m
```

### Stage 0 and initial memo: establish logical identity

Frontends produce language-independent `LogicalDAGs`. Build groups for distinct logical sub-expressions and record query roots and consumer requirements. Intern eligible identical inputs only after semantic equality and sharing legality are established; preserve keyless-node rules. P95 and P99 are different computations, so their result groups remain separate:

```text
G_input: common eligible input computation X
G95: P95 over G_input in the last 15m     root of Q1
G99: P99 over G_input in the last 15m     root of Q2
```

Current memo construction receives Pass 1 candidates. Building initial identities before Pass 1 is the proposed workflow, allowing candidate generation to reuse group identity from the start.

### Stage 1, Pass 1: add per-computation candidates

Apply computation rules to each group independently. In this example, each group receives an exact candidate and supported KLL candidates:

```text
G95 = { ExactQuantile(0.95, G_input, 15m), LocalKLLQuantile(0.95, G_input, 15m, s) }
G99 = { ExactQuantile(0.99, G_input, 15m), LocalKLLQuantile(0.99, G_input, 15m, s) }
```

Here `s` ranges over generated capacities; each candidate records its accuracy contract and dependencies. Other computations can receive exact accumulator or fused candidates. Cache generation with relevant consumer requirements and avoid composing the Cartesian product of complete workload plans.

### Stage 1, Pass 2: add intermediate groups and sharing candidates

ASAP-aware CSE recognizes that compatible KLL state supports both quantiles. Introduce or reuse a state group and add consumer candidates:

```text
G_KLL: KLL state over X[15m]
    candidates: BuildKLL(G_input, window=15m, capacity=s)

G95 += EstimateQuantile(0.95, G_KLL)
G99 += EstimateQuantile(0.99, G_KLL)
```

The result groups are not merged. Their new candidates depend on the same state group. If they attach to one physical instance, that instance's selected capacity must satisfy both; they can instead attach to different instances with different configurations. Independent candidates remain available. Conventional CSE can similarly reuse identical computations; ASAP-aware rules also expose window composition, rollup, top-k reuse, and scan widening.

For pane-based reuse, window-composition rules can add:

```text
G_panes: chain of aligned 1m KLL panes over G_input, covering 15m
G_KLL += MergeLatest15(G_panes)
```

This is another way to produce compatible window state. Other generated pane widths or compositions require coverage, merge, and accuracy checks. Groups can be added throughout rule expansion; they are not fixed after Stage 0.

### Stage 2: add physical and materialization candidates

Generate supported physical operators and assignments for these candidates. For example, an eligible `G_panes` implementation can maintain panes at ingestion while merge and estimation execute at query time. Query-time construction and supported retention modes remain candidates where legal.

Stage 2 checks deployment capabilities, ingestion eligibility, and upstream timing. It does not choose the cheapest candidate. Assignments can differ between internal nodes. Prepare finite instance slots for each group so consumers can share one instance, form several sharing subsets, or compute separately; do not enumerate all consumer partitions as complete plans. Consumer-dependent eligibility must use the selected instance's consumers; defer unknown joint checks instead of pruning a mode based on every potential consumer of the logical group.

### Stage 3: select a joint physical plan

Stage 3 covers all query roots by choosing which group instances to enable, one candidate per enabled instance, and the supplier instance for every selected input edge and query-root reference. It may select exact/local computation, a shared whole-window build, or a legal pane-based candidate. It enforces consumer/state compatibility, accuracy, latency, and resource constraints, then reconstructs and validates one `PhysicalASAPDAG`.

Each physical instance owns its work/state charges; an instance shared by several consumers is not charged once per consumer. A higher standalone cost can enable a cheaper joint plan, so earlier stages retain valid candidates and Stage 3 performs cost-based selection.

Window semantics, implementation candidates, and materialization are defined separately in [Window and Materialization Choices](window-materialization-choices.md). That design also describes pane/EH terminology, supported configurations, and materialization eligibility.

## 4. Cost Model and Optimization Objective

### Covering queries with an execution DAG

A query is covered only when its root references connect to enabled instances, every required input is recursively supplied by a compatible instance, and the resulting DAG satisfies accuracy, timing, and deployment constraints. Selecting a KLL build alone does not cover a quantile query: the estimator and the build's upstream dependencies must also be provided.

The decisions are instance activation, implementation selection, and input assignment. Sharing occurs when several consumers connect to the same instance. The objective is the total cost of those instances under their assigned demand, not the sum of independently priced query paths.

### Why MILP rather than bipartite matching

A bipartite compatibility graph captures which suppliers can serve each input, but ordinary matching cannot express the whole problem. Suppliers can serve multiple consumers, enabling a supplier has a pay-once cost, candidates can require several inputs together, and suppliers themselves depend on upstream instances. Accuracy and deployment limits further couple the choices.

For example, enabling a KLL build costs 8 and each of two estimators costs 1. Sharing costs 10. Charging the build on both assignment edges incorrectly gives 18; omitting it leaves activation unpriced. Allowing multiple assignments through b-matching or min-cost flow does not by itself model that activation cost or the dependency DAG. Facility location captures activation and assignment more closely, but must still be extended for upstream dependencies and demand-dependent costs.

MILP is the proposed joint solver, not a claim that it is the only exact method. Matching or min-cost flow can solve a restricted assignment subproblem after supplier activation, implementations, rates, and costs are fixed, provided its remaining capacities and constraints admit that formulation. Without assignment coupling, choosing each input's cheapest compatible supplier may suffice. Any local assignment must preserve the global demand and feasibility assumptions.

### Resource pricing and demand

Attribute resource usage to physical nodes in the selected DAG, then apply the existing Stage 3 cost model:

| Model | Selection policy |
| --- | --- |
| `UsageOverTime(ResourceUnitCost)` | Minimize priced usage per second. |
| `ProvisionedCapacity(ResourceUnitCost)` | Minimize priced provisioned memory, cores, and disk. |
| `ClusterCapacity(ClusterLimits)` | Enforce capacities and use `LowestLatency`, `LeastMemory`, or `ParetoFront`. |

Current memo MILP supports usage-over-time pricing; unsupported models route to legacy search. Broader MILP coverage is an extension. A Pareto objective requires frontier computation rather than one scalar solve.

Give every charge one owner:

- **Producer:** build/update, maintenance, and retained state.
- **Consumer:** local merge, extraction, read, and finalization.
- **Other physical work:** local operators and temporary state.
- **Shared resources:** raw-data retention charged once per source.
- **Query overhead:** configured work charged once per request.

Charge shared work once per actual execution or maintained state, not once per query path. Consumer-specific work remains separately charged. Use a common horizon or consistent per-second rates; distinguish memory/storage occupancy over time from peak capacity. Producer costs can depend on selected consumers, evaluation schedules, upstream materialization, and retention. Model that context rather than assuming every producer has a constant cost.

Accuracy is an end-to-end root requirement. Compose summary and window error only with valid contracts; locally admissible sketches do not prove a composed query accurate. Latency and resource limits must use the configured evaluator's timing and concurrency assumptions. Current latency accounting sums reached query-time work plus applicable compaction; this proposal does not silently replace it with a general parallel scheduler. Keep calibrated model inputs fixed during a solve.

## 5. MILP Formulation

The formulation selects a covering execution DAG. Groups represent computation contracts; instances represent physical realizations. A group can have several enabled instances with different candidates. Each enabled instance chooses exactly one candidate. Reuse is expressed by multiple consumers connecting to the same instance, without a separate producer catalog.

### 5.1 Variables

#### Sets and notation

First distinguish **how many copies may exist** from **how each copy is implemented**. Consider a group `G_KLL` representing KLL state for a specified input and window:

```text
Potential instances: P_G_KLL = {1, 2}
Candidate implementations: C_G_KLL = {small-KLL, large-KLL}

One possible selection:
    instance 1 → small-KLL → consumers A, B
    instance 2 → large-KLL → consumers C, D
```

The instance numbers carry no implementation choice. They are potential copies that the solver may enable or leave unused. Each enabled instance selects one candidate from the same implementation set; two instances may also select the same candidate. Thus `P_g` bounds the number of copies, while `C_g` defines the supported ways to implement each copy.

| Set | Meaning |
| --- | --- |
| `Q` | The workload queries. |
| `G` | Memo groups: result computations and reusable intermediate states. |
| `P_g` | Indices of potential physical instances of group `g`, such as `{1,2}`. An index identifies a copy, not its algorithm or configuration. |
| `C_g` | Supported implementation candidates for group `g`. Each candidate specifies its computation, configuration, input dependencies, and legal materialization assignment. |

Candidates refer to input **groups**; the solver later chooses the specific input **instances**. The following notation describes those dependencies:

| Notation | Meaning |
| --- | --- |
| `R(g,c)` | Input positions needed by candidate `c` of group `g`. Keep two positions distinct even if both require the same group. |
| `H(g,c,e)` | The group whose output is needed at input position `e`. |
| `K(g,c,e)` | Candidates of that input group that can supply the required output, configuration, and guarantees. |
| `T(q)` | References to computation results needed to produce query `q`'s output. `root(q,t)` identifies the group for reference `t`. A scalar query can reference several computation groups. |

For example, a KLL estimator has an input position `state`. `H(G95, estimator, state) = G_KLL`; `K(G95, estimator, state)` lists the KLL-state candidates that meet this estimator's requirements. Its input may connect to instance 1 or 2, provided that instance selects a compatible candidate. Likewise, a selected KLL build has its own input position referring to the input-data group.

Capacity, pane width, retention, and materialization are candidate configurations. Finite configurations permit lookup-table costs without assuming a linear size/cost function.

#### Decision variables

```text
u[g,i]           ∈ {0,1}   enable instance i of group g
x[g,i,c]         ∈ {0,1}   instance (g,i) selects candidate c
b[g,i,c,e,j]     ∈ {0,1}   input edge e connects to instance j of H(g,c,e)
r[q,t,i]         ∈ {0,1}   root reference t of query q uses instance i
```

Each instance's materialization can be bundled into its candidate or represented by consistent legal assignment variables. Several internal nodes can have separate assignment slots. Two enabled instances may use the same candidate but have different consumers or execution scopes.

#### Objective and cost attribution

For fixed-context costs:

```text
min  Σ_{g,i,c} cost[g,i,c] × x[g,i,c] + shared_resource_cost + query_overhead
```

The cost belongs to the instance's local work/state, excluding input instances. A KLL-state instance owns build and retention; estimator instances own extraction. A reused merge result also has its own instance. Two consumers sharing one state instance incur its owned charge once; two enabled state instances incur two charges.

`shared_resource_cost` accounts for resources owned outside computation instances, such as retained raw data charged once per source. If selected scans require 5m and 15m of the same raw source, that source's required horizon is 15m, not 20m; derive its storage cost from a retention variable constrained by the selected scans. Intermediate-state retention already charged to an instance is excluded here. `query_overhead` is any configured once-per-request charge, counted at each query's request rate and excluded from instance costs; it is zero when the model has no such charge. Both terms must have explicit model expressions, not arbitrary residual adjustments after solving.


An enabled instance is not necessarily executed only once: query-time work can repeat. Its materialization, consumer schedules, phases, range steps, and upstream consumers determine the execution rate and state lifetime. Extend the base model with reach/rate/lifetime variables or finite context configurations constrained to match the selected graph. Do not assign a fixed cost independent of that context. Raw retention uses shared resource variables; request overhead uses query-level demand.

### 5.2 Constraints

#### Instance selection and query coverage

```text
Σ_{c ∈ C_g} x[g,i,c] = u[g,i]                  for each instance (g,i)
Σ_{i ∈ P_root(q,t)} r[q,t,i] = 1               for each root reference (q,t)
r[q,t,i] ≤ u[root(q,t),i]
```

There is no constraint limiting an entire group to one enabled instance. Each query-root reference must connect to an available implementation.

#### Input dependencies and compatibility

For each input edge of a selected candidate, choose one compatible input instance. Let `h = H(g,c,e)`:

```text
Σ_{j ∈ P_h} b[g,i,c,e,j] = x[g,i,c]
b[g,i,c,e,j] ≤ u[h,j]
b[g,i,c,e,j] ≤ Σ_{d ∈ K(g,c,e)} x[h,j,d]
```

Several consumers can choose the same `j`; other consumers can choose another instance of the same group. Compatibility covers state family, grouping, reduction, window coverage, capacity, timing, and freshness. If two consumers attach to one instance, its selected candidate must satisfy both consumers.

Every enabled instance must have a selected root reference or incoming input edge:

```text
u[h,j] ≤ Σ root references using (h,j)
       + Σ selected input edges using (h,j)
```

Together with acyclicity, this prevents disconnected instances even when prices are zero. Enforce an acyclic physical instance graph, not a requirement that every possible memo dependency be acyclic. For example, with `N` total slots and order variables `0 ≤ order[g,i] ≤ N−1`:

```text
order[h,j] + 1 ≤ order[g,i] + N × (1 − b[g,i,c,e,j])
```

This orders producers before their selected consumers. Interchangeable slots may additionally use symmetry-breaking constraints such as `u[g,i] ≥ u[g,i+1]`.

#### Fusion and available outputs

A fused candidate can eliminate internal computation boundaries without exposing those intermediate results. In the base model it implements its owning group; only explicitly available outputs can serve external consumers. For example, a fused `topk(sum(...))` does not automatically supply another query's full `sum(...)` result. That query can require a separate sum instance. Multi-output fusion needs explicit output mappings before it can satisfy several groups; absorption alone is not coverage.

#### Accuracy, materialization, and resources

- **Accuracy:** preserve state contracts and enforce root-level composition bounds, feasible-combination tables, or sound refinement over the selected instance graph.
- **Physical validity:** require supported operators, legal materializations, available upstream state, and valid freshness/lifetimes.
- **Windows:** validate alignment, coverage without omission/double counting, retention, and algorithm-specific guarantees.
- **Resources and latency:** apply configured CPU/memory/disk limits, query deadlines, and recurrence checks using actual instance demand and timing.

For additive, fixed-context resource usage:

```text
Σ_{g,i,c} usage[k,g,i,c] × x[g,i,c]
  + shared_resource_usage[k] ≤ capacity[k]
```

`shared_resource_usage[k]` covers non-instance resources such as per-source raw-data storage, without repeating instance-owned usage. Peak capacity is not average occupancy; temporary lifetimes and concurrency need their own accounting. Use bounded auxiliary-variable linearizations where needed. A sum of fixed candidate costs is only the base case, not a substitute for the configured evaluator.

## 6. Worked Example: Shared KLL for P95/P99

```text
Q1: quantile_over_time(0.95, stream[15m]), every 1m
Q2: quantile_over_time(0.99, stream[15m]), every 1m
```

Using the groups introduced in Section 3, compare the cost of selecting shared versus local KLL candidates:

```text
G_input[i0] → G_KLL[i1]: build KLL state
                    /              \
          G95[i1]: estimate P95  G99[i1]: estimate P99
                   |                   |
                  Q1                  Q2
```

Illustrative costs per common horizon, assuming all listed candidates satisfy requirements:

| Choice | Build cost | Consumer costs | Total |
| --- | ---: | ---: | ---: |
| Separate KLL builds | 8 + 8 | 1 + 1 | 18 |
| Shared KLL build | 8 | 1 + 1 | 10 |
| Separate exact computations | 12 + 12 | included | 24 |

The shared choice enables one state instance, and both estimator instances connect to it:

```text
u[G_KLL,1] = x[G_KLL,1,build-s] = 1
x[G95,1,KLL-estimator] = x[G99,1,KLL-estimator] = 1
b[G95,1,KLL-estimator,state-input,1] = 1
b[G99,1,KLL-estimator,state-input,1] = 1
```

The build cost is owned by `G_KLL[1]`; each estimator instance owns its extraction cost. If accuracy requires larger state, the solver compares compatible shared capacity, local states, and exact candidates. Quantile level alone does not determine capacity.

With four consumers, the same group can also have two shared instances:

```text
G_KLL[1]: small-capacity query-time state ← consumers A, B
G_KLL[2]: large-capacity query-time state ← consumers C, D
```

Both can be enabled if their combined cost is lower or one instance cannot satisfy all requirements. Their selected candidates can differ in capacity, materialization, retention, and execution frequency. Each instance must satisfy every attached consumer; consumers do not all have to share one configuration.

For ingestion-time reuse, a supported implementation maintains 1m KLL panes and merges the latest 15 at evaluation. Both consumers can share panes and an eligible merge result. A direct whole-window sketch cannot be assumed to delete expired samples. Different window lengths or pane widths require separate compatibility and coverage checks.

## 7. Search-Space Reduction and Validation

Reduce search without losing valid joint choices:

1. Intern duplicate identities and cache generation with complete relevant context.
2. Remove provably invalid candidates early; apply cost dominance at Stage 3 only when external sharing interfaces and resource obligations are preserved.
3. Split independent workload components only when all possible sharing, dependencies, raw retention, and constraints separate. Global capacities or calibration can prevent independent optimization.
4. Use a validated baseline incumbent and solver time/gap limits. Restricted parameter inventories narrow the optimality claim; enrichment requires solving the expanded inventory.

Define a finite rule/configuration inventory and process new groups/candidates through a worklist, including newly enabled matches with existing candidates, until no eligible rule produces a new item. Deduplicate rule applications and do not discard candidates merely because they are more expensive alone. If generation stops early, report the restricted inventory rather than claiming rule closure.

Instance slots also bound the search space. Use an upper bound justified by the unshared expansion of the permitted finite, acyclic derivations and evaluation scopes, or explicitly declare a slot cap as a restriction. An arbitrary cap can omit profitable sharing partitions. Rule closure establishes generation completeness only within this declared inventory; MILP optimality establishes selection quality within the generated model. Validate both separately.

Reconstruct enabled instances, their candidates, and selected input/root connections into one `PhysicalASAPDAG`. Do not merge distinct instances solely because they share a group identity. Check dependencies and cycles, then run the configured accuracy, latency, and cost/resource evaluators. Return a validated plan, fallback, or explicit search failure status.

A complete MILP does not require LBBD. Current code uses a lower-bound model with lazy refinement for difficult costs/checks. When refinement is needed, add only sound feasibility or cost cuts conditioned on every relevant decision. Validation of one plan alone does not prove a relaxed model optimal. Report the best validated cost, certified lower bound/gap, solver status, and stop reason. Timeout without a plan is not proof of infeasibility.

## 8. Implementation and Evaluation

Reuse existing structures rather than create a parallel optimizer framework; proposed names and extensions are marked below:

| Structure | Responsibility |
| --- | --- |
| `MemoGroup`, `Memo` | Result/state contracts, dependencies, roots, and per-group candidates. |
| `ImplementationCandidate` | Local nodes, fusion absorption/output metadata, child groups, contracts, and reuse requirements. |
| `SharedDecision`, `SharedDecisionKind` (proposed names) | Configuration choices coordinated across candidates; migrate state-local choices into instance candidates and retain cross-instance decisions where needed. |
| MILP `Choice` and proposed instance records | Instance identity, selected physical/materialization assignment, consumer connections, and cost context. |
| `PhysicalASAPDAG`, `Selection`, reports | Selected output, validation, bounds, and search status. |

`SharedDecision` replaces the design term `CouplingSite`; `SharedDecisionKind` replaces `CouplingKind`. Current Rust code still uses the latter names. A shared decision has an identity and kind, with legal configurations and compatibility checks. Use **configuration**, rather than **threshold**, because pane layouts and scan-label sets need not be scalar or ordered. This is a proposed naming change, not a claim that the Rust types have already been renamed.

This design extends the current shared-decision representation. Introduce intermediate-state contracts, finite instance slots, and consumer compatibility; migrate state choices into instance candidates and assign costs to physical instances. This requires changes to generation, selection, decoding, and pricing; it also changes behavior beyond the proposed type rename. Raw retention and global capacities remain accounting constraints.

Current main places memo/rules in `search/memo` and solver code in `search/milp`, with Stage 2 physical generation and Stage 3 pricing remaining authoritative. `Legacy` is the default; `MemoBruteForce` is the reference and `MemoMilp` the solver path. Early memo construction, unified shared-state groups with selectable physical instances, broader cost-model support, EH/direct sliding algorithms, and arbitrary pane hierarchies are proposed extensions.

### Implementation phases

| Phase | Deliverable and acceptance criterion |
| --- | --- |
| 1. Memo generation | Move identity construction before Pass 1 and generate candidates directly into groups without losing semantics, consumer obligations, or fusion. |
| 2. Shared-state groups | Extend group contracts; generate intermediate groups, instance slots, and compatible consumer connections with legal materializations and per-instance cost ownership. |
| 3. Joint selection | Encode instance activation, implementation selection, input connections, and demand to cover all queries; match an independent tiny-instance enumerator under the same inventory/models. |
| 4. Validation and scaling | Validate reconstructed plans, prove safe reductions, and measure generation/model/solve costs under budgets. |

### Evaluation

**Correctness:** test identity versus sharing, producer compatibility, per-instance cost attribution, active dependencies, fusion with external intermediate consumers, multiple input edges, multiple configurations/partial-sharing subsets of one group, zero-price resources, pane boundaries, legal materializations, end-to-end accuracy, and capacity/status handling. Validate any relaxed bounds and cuts against enumerated plans.

**Optimality:** compare MILP with an independent tiny-instance enumerator covering instance activation, candidate selection, root/input assignments, and dependency validity under the same inventory/models. The existing group-level brute-force reference must be extended before it can verify arbitrary instance partitions. Test candidate-generation coverage separately, including cases where two independently shared instances outperform full sharing and separate computation. Use an explicit numerical/tie policy.

**Scalability:** measure groups, candidates, instance slots/enabled instances, state configurations, input connections, variables/constraints, generation/solve/validation time, and peak planner memory. Compare joint versus independent planning savings, time-limited gaps, and modeled versus validated costs across query overlap, window configurations, and materializations.
