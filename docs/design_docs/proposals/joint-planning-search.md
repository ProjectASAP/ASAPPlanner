# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design and implementation contract. The candidate-connection pipeline is implemented on the migration branch; acceptance is in progress. Direct linear encodings below also describe extensions where production currently relies on evaluator refinement. See the [implementation plan](joint-planning-implementation-plan.md) for current scope and verification gates.

## 1. Motivation and Challenges

ASAP primitives increase both the opportunities for query optimization and the difficulty of searching for a good plan. A quantile, for example, may be computed directly or estimated from a summary. Several quantiles may use the same summary, and several windows may use overlapping sets of panes. The cost of one query therefore depends on decisions made for other queries. Selecting the cheapest implementation of each query independently can miss the cheapest workload plan.

ASAPPlanner also optimizes for **workload patterns**, including repeated queries, their evaluation frequencies and alignment, and overlap among their windows. The unit of optimization is therefore a workload over time, not just a set of queries executed once. These patterns introduce further choices about when to recompute, which consumers can share an execution, and which intermediate states to maintain or retain across evaluations.

For example, consider two queries over the same input and aligned 15m window:

```text
Q1: P95(X[15m]), evaluated every 1m
Q2: P99(X[15m]), evaluated every 5m
```

Over 60 aligned minute ticks, separate query-time KLL builds require 60 + 12 executions. A compatible shared build can serve both queries at the 12 common query evaluation events, reducing the total to 60 builds. Another supported candidate may maintain 1m panes at ingestion and merge them at evaluation, trading update and retained-state costs against repeated whole-window construction. Eligible retained-state candidates introduce refresh and lifetime choices as well. Repetition alone does not make an earlier window's state valid for a later window; reuse must preserve coverage and freshness.

The search space therefore grows across algorithm, configuration, consumer sharing, materialization, and temporal reuse choices together. Selecting a producer's consumers can change its execution frequency and required retention, so these decisions cannot be priced independently. Explicitly expanding every query evaluation event can further multiply the optimization model's size, even when query texts and algorithm candidates remain unchanged. Workload-pattern optimization thus adds both a combinatorial planning challenge and a need for compact representation of repeated execution demand.

The problem addressed here is to find a minimum-cost execution DAG that produces every requested result and satisfies accuracy, latency, and resource requirements. The optimizer must account for shared computation and retained data without charging them repeatedly along different query paths. It must also keep its own search time and memory manageable as the number of queries and implementation choices grows.

The following sections develop a memo-based representation and a joint selection procedure for this problem. The proposal concerns the finite candidates supported by the planner and its configured cost and constraint models. It does not introduce new sketch or window algorithms, or a runtime scheduler. An optimal solution consequently means an optimum within the generated model, not among all imaginable execution plans.

## 2. Traditional Memo Groups and Rule Expansion

A memo group represents expressions with the same logical result. Expressions reference input groups, allowing the optimizer to retain several ways of computing a result without copying complete plans. Graefe's [Cascades framework](https://15721.courses.cs.cmu.edu/spring2016/papers/graefe-ieee1995.pdf) develops this representation with rule-driven exploration and optimization.

Consider a filter above an inner join:

```text
G_A    = { Scan(A) }
G_B    = { Scan(B) }
G_join = { Join(G_A, G_B) }
G_out  = { Filter(A.x > 10, G_join) }
```

A valid filter-pushdown rule adds:

```text
G_filtered = { Filter(A.x > 10, G_A) }
G_out += Join(G_filtered, G_B)
```

The new join belongs to `G_out` because its result is unchanged. Implementation rules then introduce physical candidates:

```text
Join(G_filtered, G_B)
    → HashJoin(G_filtered, G_B)
    → MergeJoin(G_filtered, G_B)
    → NestedLoopJoin(G_filtered, G_B)
```

These candidates differ in cost and input requirements; merge join, for example, requires ordered inputs. Optimization can cache `Best(group, required_properties)`. Logical and physical exploration need not occur in separate exhaustive phases: Cascades explores expressions as needed and avoids repeating earlier work.

This reuse concerns optimization. A shared group does not itself require shared runtime execution. ASAPPlanner needs an explicit execution-sharing decision, developed next.

## 3. ASAPPlanner Memo Groups and Rule Expansion

ASAPPlanner retains the distinction between a required output and the ways to produce it. A **group** describes a requested result or an intermediate-state contract. Its identity includes input, filters, grouping, and window semantics; a state group also specifies its interface and guarantees. A **candidate** describes a supported implementation of that contract. At the physical level it includes operators, configuration, materialization, execution scope, and required inputs.

Approximation requires care in interpreting the group. An exact quantile and a KLL estimate target the same requested result, but need not return identical values. The approximate candidate is admissible only under the applicable accuracy requirements. Thus the proposed result/state contracts extend traditional exact equivalence rather than asserting equivalence between exact and approximate values.

The second extension concerns selection. Several physical candidates in one group may be selected, and each selected candidate becomes a node in the execution DAG. Consumers connect to particular candidates. For example:

```text
G_KLL
    C_small: small-capacity KLL ← consumers A, B
    C_large: large-capacity KLL ← consumers C, D
```

Selecting both candidates permits two sharing subsets. Selecting only one is appropriate when its configuration and cost suit all consumers. This representation uses no additional physical-instance index: a candidate is the selectable unit of execution.

That simplification places a requirement on generation. If computations must coexist with separate execution scopes or incompatible input bindings, they need distinct generated candidates, even when their algorithms and capacities match. One selected candidate has one set of input bindings. Removing instance indices therefore does not establish that every duplicate computation can safely be shared, or that an arbitrary candidate inventory is complete.

The workflow follows [planner layering](planner-layering.md). Local group candidates represent components of the workload DAGs described there.

### Stage 0: build initial groups

The frontend produces normalized logical expressions. The memo builder interns equivalent sub-expressions and records their query roots. Consider two queries over the same input and evaluation schedule:

```text
Q1: P95(X[15m]), every 1m
Q2: P99(X[15m]), every 1m

G_input: input X
G95: P95 over X[15m]     root of Q1
G99: P99 over X[15m]     root of Q2
```

P95 and P99 remain different groups. The production driver builds initial logical groups before generating Pass 1 candidates, then populates those groups from the supported per-computation inventory. The API accepting an existing Pass 1 inventory constructs initial identities from its roots.

### Stage 1, Pass 1: expand computation candidates

Computation rules expand each result group with the supported ways to obtain its output:

```text
G95 = { ExactQuantile(0.95, G_input), LocalKLLQuantile(0.95, G_input, s) }
G99 = { ExactQuantile(0.99, G_input), LocalKLLQuantile(0.99, G_input, s) }
```

Here the window remains 15m, and `s` ranges over generated capacities. Each candidate records dependencies and accuracy obligations. Rules can add groups and candidates without enumerating complete workload combinations.

### Stage 1, Pass 2: expose shared state

ASAP-aware CSE recognizes that both quantiles can consume compatible KLL state:

```text
G_KLL: KLL state over X[15m]
    candidates: BuildKLL(G_input, capacity=s)

G95 += EstimateQuantile(0.95, G_KLL)
G99 += EstimateQuantile(0.99, G_KLL)
```

Conventional CSE discovers identical computations. ASAP-aware CSE also exposes common intermediate state for different results. It creates or reuses a state group without merging `G95` and `G99`.

Window rules can similarly introduce a pane-state group and a merge candidate:

```text
G_panes: aligned 1m KLL panes covering 15m
G_KLL += MergeLatest15(G_panes)
```

### Stage 2: expand physical candidates

Physical rules provide executable operators, configurations, and materialization assignments for the generated computations. For example, a supported pane implementation can maintain state at ingestion and merge it at evaluation. This introduces two timing decisions within the same query, rather than one materialization choice for the whole query. Eligibility that depends on consumers is checked against the selected connections; considering every potential consumer at once could incorrectly reject a useful candidate.

For the running example, assume evaluations occur at aligned minute boundaries and the deployment supports ingestion-time pane maintenance. Stage 2 can expand the state groups as follows; `s` is a generated KLL capacity:

```text
Group     Physical candidate     Computation                 Materialization
G_panes   P_ingest(s)             Maintain aligned 1m panes   MatAtIngestion(Memory)
G_KLL     K_merge(s)              MergeLatest15(G_panes)      MatAtQuery(Memory)
G_KLL     K_build(s)              BuildKLL(G_input, 15m)      MatAtQuery(Memory)
G95       E95                    EstimateQuantile(.95, G_KLL) NotMaterialized
G99       E99                    EstimateQuantile(.99, G_KLL) NotMaterialized
```

`P_ingest(s)` retains the panes needed to cover the 15m window. `K_merge(s)` constructs window state from those panes at evaluation; `K_build(s)` instead builds window state from available raw input. Each candidate records its input requirements and costs. The estimators can use either compatible state candidate. Exact and local KLL candidates from Pass 1 also receive supported physical implementations and remain available. Stage 2 has generated choices; it has not yet selected a build or connected an estimator to it.

Window semantics, algorithms, and materialization remain separate dimensions. Their definitions and support restrictions belong in [Window and Materialization Choices](window-materialization-choices.md).

### Stage 3: select and connect candidates

Selection chooses physical candidates and connects their required inputs until every query root is covered. Several consumers may connect to one KLL candidate or to different candidates from `G_KLL`. The selected graph is reconstructed as a `PhysicalASAPDAG` and validated. This is where implementation costs are compared jointly; the earlier rules expose the choices on which that comparison depends.

Suppose the pane-based choice has the lowest modeled cost among feasible plans, and capacity `s*` satisfies both queries after pane merges. Stage 3 selects `P_ingest(s*)`, `K_merge(s*)`, `E95`, and `E99`, together with their required upstream input, and chooses these connections:

```text
Selected input for X
        │
        ▼
P_ingest(s*)              maintain 1m panes at ingestion
        │
        ▼
K_merge(s*)               merge the latest 15 panes at evaluation
        ├───────────────┐
        ▼               ▼
       E95             E99
        │               │
        ▼               ▼
       Q1              Q2
```

For a common evaluation of Q1 and Q2, the selected merge state serves both estimators. Pane maintenance is charged once for the shared pane chain, and merge work once per common evaluation; each estimator contributes its own extraction cost. `K_build(s*)` and the exact/local candidates are unselected in this example. If timing or accuracy prevents this sharing, Stage 3 may connect the estimators to different candidates from `G_KLL`. Thus Stage 2 enumerates executable choices, while Stage 3 determines which choices execute and which consumers share them.

## 4. Cost Model

The cost of a shared plan is obtained from its selected execution nodes, not by summing independently priced query paths. Each node owns its local work and state, excluding its inputs. In the running example, the KLL build owns construction and retained state, while each estimator owns extraction. Two estimators using one build therefore incur one build charge and two extraction charges.

**Charging a selected candidate once does not mean executing it once.** It means assigning its cost to one owner rather than repeating that cost for every consumer. If a shared KLL build is reconstructed every minute, its charge includes every reconstruction over the accounting horizon. For 60 common evaluations of P95 and P99, the build work costs `60 × B`, where `B` is one reconstruction's cost—not `B`, and not `120 × B`.

The candidate's materialization, selected consumer schedules, and lifetime determine its execution count and state occupancy. When those quantities depend on selected connections, its cost must depend on those connections as well; it cannot be a constant fixed before selection. For example, a query-time build serving only a five-minute consumer executes 12 times over 60 aligned minute ticks. Connecting an additional one-minute consumer can increase that count to 60, with sharing at common ticks when legal. The model must derive this demand from the selected graph using execution-count/lifetime variables or consistent finite context configurations. All terms use a common horizon or consistent rates, so ingestion work and evaluation work can be compared.

Some charges belong to a resource shared beyond one computation. If two consumers need 5m and 15m of the same raw source, the required retention horizon is 15m, not 20m. Represent this shared storage as a state group `G_raw[d]` for source `d` and a particular storage scope, with candidates such as `Retain5m` and `Retain15m`. The group describes access to retained raw data; each candidate advertises its coverage, and each consumer connects only to a candidate covering its required horizon. Both consumers can use `Retain15m`, whose storage cost is charged once through candidate selection. Reads and intermediate KLL state retain their own cost owners.

The current evaluator owns shared raw retention; explicit retention candidates above describe a direct encoding rather than an additional implemented storage charge.

Fixed request overhead does not affect plan selection. If query `q` executes `N_q` times over the accounting horizon with fixed per-request cost `h_q`, add `Σ_q N_q × h_q` to the reported total after optimization. Its resource usage still counts against applicable budgets. Any overhead that changes with implementation or connections belongs in the corresponding decision-dependent cost model instead.

The following cases make the accounting concrete. Let `B` be the cost of one whole-window KLL build, `M` one pane merge, and `E95` and `E99` the extraction costs. These are illustrative work charges; retained and temporary resources are accounted for separately over the same horizon.

| Case | Selected execution | Cost accounting |
| --- | --- | --- |
| Independent query-time builds | Q1 and Q2 each build KLL state and extract a quantile. | For one evaluation each: `2B + E95 + E99`. The two builds are separate work. |
| Shared query-time build | Q1 and Q2 use one compatible build in a common evaluation. | `B + E95 + E99`. Query-local state is charged for its actual lifetime. |
| Repeated common evaluations | Both queries evaluate together `n` times; state is discarded after each evaluation. | `n × (B + E95 + E99)`. Selecting one candidate does not mean building only once over the entire horizon. |
| Different evaluation timestamps | Q1 evaluates every minute; Q2 every five minutes on the same grid. Both use a 15m window. | Over 15 evaluation ticks, a shareable query-local build executes 15 times, with 15 P95 extractions and 3 P99 extractions: `15B + 15E95 + 3E99`. This assumes sharing at the three common timestamps; an earlier window state cannot automatically answer a later window. |
| Ingestion-maintained panes | One pane chain feeds a shared query-time merge and both estimators. | Ingestion updates and pane retention are charged once for the chain. Add `n × (M + E95 + E99)` for `n` common evaluations, plus query-local merge memory. |
| State retained across evaluations | An eligible pane-chain candidate uses `MatAtQueryKeptAcrossEvaluations(Memory)`. | Charge initial construction, required refresh/update work, retained memory/storage over its lifetime, and evaluation work. Reuse does not eliminate freshness maintenance; generate this case only when supported. |
| Multiple selected candidates in one group | Some consumers use a small KLL state and others a separate large state. | Charge each selected state's build/update and retention, then each consumer's local work. Equal group identity does not collapse two physical candidates into one charge. |
| Shared raw-data retention | Two consumers need 5m and 15m of the same raw source and compatible retention scope. | Both consumers connect to `Retain15m` in `G_raw[d]`. Charge that candidate once; reads and intermediate KLL state have separate owners. |
| Per-request overhead | Two requests share computation but still require separate response handling. | Report fixed overhead as `Σ_q N_q × h_q`; omit this constant from the optimization objective but retain its applicable resource usage. Decision-dependent overhead stays in the optimization model. |

Memory-time and capacity answer different questions. A state occupying `m` bytes for duration `t` contributes `m × t` to memory-time. A capacity limit instead considers simultaneous residency, including temporary allocations; it cannot be inferred from memory-time alone.

The configured Stage 3 objective and evaluators remain authoritative. The candidate-connection route supports usage-over-time and provisioned-price objectives, and cluster-capacity latency/memory priorities and Pareto traversal. Scalar cost bounds apply only where certified; bounded objective/frontier traversal reports incompleteness. Accuracy is an output requirement, since locally acceptable errors may compose into an unacceptable query result. Latency and resource limits likewise depend on the configured timing and capacity assumptions.

## 5. MILP Formulation

The selection problem combines producer activation with dependency assignment. Ordinary bipartite matching describes compatible pairs, but does not directly capture a producer paid for once, used by several consumers, and itself dependent on other producers. A MILP expresses these decisions together. The following formulation gives the fixed-context base; demand-dependent costs require rate/lifetime variables or finite context configurations consistent with the selected graph.

### 5.1 Variables

#### Candidates and Dependency Connections

The optimizer needs to know which computations are available, which inputs each computation requires, and which query outputs must be produced. These are the nodes, dependencies, and roots of the candidate dependency graph. For the running example, suppose both KLL capacities can serve P95, but only the larger capacity meets P99's accuracy requirement:

```text
Groups and their candidates:
    G_KLL: {small-build, large-build}
    G95:   {exact-P95, estimate-P95}
    G99:   {exact-P99, estimate-P99}

Allowed producer–consumer connections (producer → consumer):
    small-build ────────→ estimate-P95
    large-build ────────→ estimate-P95
    large-build ────────→ estimate-P99

Query roots:
    Q1 ← exact-P95 or estimate-P95
    Q2 ← exact-P99 or estimate-P99
```

The builds and exact candidates also require raw-input dependencies, omitted from this diagram. The graph records possible connections; it does not select them. Choosing both estimators and connecting them to `large-build` produces one shared build in the selected execution DAG. The candidate dependency graph may contain cycles among possible choices; the selected execution DAG must be acyclic.

| Name | Meaning | Example |
| --- | --- | --- |
| `Groups` | Logical result and state groups. | `{G_KLL, G95, G99, ...}` |
| `Candidates(g)` | Finite physical candidates in group `g`. | `Candidates(G95) = {exact-P95, estimate-P95}` |
| `ProducerRequirements(consumer)` | Dependencies that the consumer must have supplied. | `ProducerRequirements(estimate-P95) = {KLL-state}` |
| `CompatibleProducers(consumer,requirement)` | Producer candidates allowed to satisfy that dependency. | `CompatibleProducers(estimate-P99,KLL-state) = {large-build}` |
| `QueryRoots` | Required query outputs. | `{Q1, Q2}` |
| `RootCandidates(q)` | Candidates allowed to provide query root `q`. | `RootCandidates(Q1) = {exact-P95, estimate-P95}` |

Candidate IDs are globally unique; `Candidates` denotes their union across groups. A producer requirement describes what must be supplied, not a producer already selected. For example, P95 requires KLL state; either compatible build may satisfy it. Requirements remain distinct: a join needs both `left` and `right`, even if the same producer can supply both. For this formulation, each query has one explicit output root. If its output combines several computations, a finalization candidate has those computations as required inputs; covering the root therefore covers all of them.

A candidate can be a producer on one connection and a consumer on another. Three binary variables select nodes, producer–consumer connections, and query-root assignments:

```text
x[c]                                      ∈ {0,1}   select candidate c
connect[producer,consumer,requirement]     ∈ {0,1}   producer satisfies this consumer requirement
r[q,c]                                    ∈ {0,1}   candidate c provides query root q
```

Connection variables exist only for compatible producer–consumer requirements. Capacity and materialization belong to candidate configuration, so selection needs no separate copy or instance variables.

#### Pane width is an optimization decision

The query's window width is fixed semantics. The pane width is a physical decision, selected jointly with retention, summary capacity, materialization, and consumer connections. Do not restrict the search to a few manually chosen widths and describe the result as optimal over all supported pane widths.

For the supported whole-pane, aligned reconstruction algorithm, use the declared time resolution (currently integer milliseconds). Derive the complete legal width set from the window boundaries and evaluation grid. For example, a 15m window evaluated every 1m admits positive integer-millisecond divisors of 60,000, subject to alignment, offset, representable pane counts, and physical eligibility. A 5m pane alone cannot serve every minute endpoint. Different compatible reader subsets can select different widths.

This finite domain has an exact MILP encoding: activate width-specific configurations with binary decisions. If a single chain is represented by mutually exclusive width configurations, `sum_d u[d] = active` and its selected width is `sum_d d*u[d]`. The candidate graph encodes the same choice through configuration-specific candidate activations and compatible connections. Multiple chains may be active, so there is no single workload-wide pane-width variable.

Precompute each configuration's local resource estimates and reader compatibility. A free integer width by itself would leave nonlinear terms such as `window_width / pane_width` and arbitrary measured size/cost functions; it would not make the current model a valid MILP. Complete discrete encoding avoids that issue. Prune widths only with a proof covering costs, accuracy, and sharing interfaces; a candidate-count cap must be reported as a restricted domain.

A proven optimum applies to all generated legal widths and the other configured finite decisions under the encoded model. A planning-budget stop returns a validated incumbent and an available certified bound, not a globally optimal pane width. This does not claim optimality over unsupported boundary reconstruction, new window algorithms, or every possible sketch capacity.

#### Auxiliary variables: `order`, `run`, and `need`

These are solver variables, not functions to be evaluated before optimization. Square brackets identify which candidate, connection, or query evaluation event a variable belongs to. Their indices are:

| Index or parameter | Definition |
| --- | --- |
| `c`, `producer`, `consumer` | Globally unique physical candidate IDs in `Candidates`. |
| `requirement` | One dependency in `ProducerRequirements(consumer)`, such as `KLL-state` or a join's `left` input. |
| `N` | The number of physical candidates, `|Candidates|`. |
| `Ticks` | A finite set of query evaluation events over the accounting horizon. |
| `t` | One query evaluation event in `Ticks`; it identifies an evaluation context, not a solver-chosen start time. |
| `evaluates[q,t]` | A fixed binary parameter: 1 if query `q` evaluates at query evaluation event `t`, otherwise 0. |

A shared query evaluation event requires compatible evaluation contexts, not merely equal wall-clock timestamps. The variables have the following meanings:

| Variable | Domain and indexing | Definition |
| --- | --- | --- |
| `order[c]` | Continuous, `0 ≤ order[c] ≤ N−1`, for each candidate `c`. | A topological rank used to require each selected producer to precede its consumers in the dependency graph. It is not execution time, latency, or a runtime schedule. |
| `run[c,t]` | Binary, for each candidate `c` and query evaluation event `t`. | 1 exactly when the selected graph requires candidate `c` to execute for query evaluation event `t`; 0 otherwise. This records execution demand, not measured runtime activity. |
| `need[producer,consumer,requirement,t]` | Binary, for each compatible producer–consumer requirement and query evaluation event `t`. | 1 exactly when that connection is selected **and** the consumer executes for query evaluation event `t`; 0 otherwise. It records demand along one connection. |

For example, let `build` be a KLL producer, and `P95` and `P99` its estimator candidates. At query evaluation event `t5`, suppose both estimators execute and both use `build` for their `KLL-state` requirement:

```text
x[build] = 1                         build belongs to the selected plan
run[P95,t5] = run[P99,t5] = 1         both estimators execute for this query evaluation event
need[build,P95,KLL-state,t5] = 1      the P95 connection demands the build
need[build,P99,KLL-state,t5] = 1      the P99 connection also demands it
run[build,t5] = 1                    one shared build execution, not two
```

At another query evaluation event with no query or consumer demand, `run[build,t] = 0` even though `x[build] = 1`. Thus `x` selects a plan node, `need` records per-connection demand, and `run` combines demand into one execution per compatible query evaluation event. The constraints in Section 5.2 enforce these definitions, including the logical AND defining `need`.

For the subgraph `build → P95` and `build → P99`, valid ranks are `order[build] = 0` and `order[P95] = order[P99] = 1`. Ranks need not be unique or integral. They have no execution meaning for unselected candidates. `order` variables can be omitted if the candidate dependency graph is already acyclic; otherwise the ordering constraints in Section 5.2 exclude selected cycles.

The `run`/`need` model assumes query-time recomputation on a fixed evaluation schedule: each required producer executes for the same query evaluation event as its consumer, and compatible consumers can reuse that execution. The query evaluation event is a logical evaluation, not a claim that all operators start simultaneously. Ingestion maintenance and retained state need their own update/freshness/lifetime constraints; this model must not be applied to them unchanged.

#### Objective

For candidates whose execution demand and lifetime are fixed, the objective is:

```text
min Σ_c local_cost[c] × x[c]
```

Each selected candidate owns its local work and state, excluding producers. This includes shared-resource candidates such as `Retain15m`, so no separate shared-resource charge is needed. Fixed request overhead is omitted from the objective because it is constant for the workload; Section 4 describes its inclusion in reported total cost. Where selected consumers or connections change cost, additional expressions model that dependence rather than treating it as a constant.

Here `local_cost[c]` covers the entire accounting horizon, not just one invocation. If selected connections change demand, the fixed-coefficient expression above is insufficient. The MILP must instead include the resulting execution and retention costs, with constraints tying execution counts and lifetimes to the selected connections. Selecting `x[c] = 1` alone determines neither the number of executions nor their total cost; the same accounting applies to resource limits.

For the query-time demand model, replace the fixed-context objective with:

```text
min Σ_c standing_cost[c] × x[c]
  + Σ_{c,t} execution_cost[c,t] × run[c,t]
```

`standing_cost` contains only horizon-wide costs independent of execution count; `execution_cost` contains local per-execution costs, excluding producers. They partition the modeled charges rather than adding them to `local_cost` again. Any connection-dependent retention or peak-memory cost requires additional constraints.

### 5.2 Constraints

A feasible plan must cover every query, meet every query's accuracy target, provide executable implementations and valid materializations, and respect configured CPU, memory, disk, and latency limits. Dependency, execution-demand, configuration, and acyclicity constraints below make those requirements consistent across shared computation. Limits are imposed when configured; a missing limit does not mean that its resource has zero cost.

#### Cover every query output

```text
Σ_{c ∈ RootCandidates(q)} r[q,c] = 1        for each q ∈ QueryRoots
r[q,c] ≤ x[c]
```

#### Satisfy every selected consumer's producer requirements

```text
Σ_{producer ∈ CompatibleProducers(consumer,requirement)}
    connect[producer,consumer,requirement] = x[consumer]

connect[producer,consumer,requirement] ≤ x[producer]
```

The equality applies to every `requirement ∈ ProducerRequirements(consumer)`: each selected consumer must have every dependency supplied by one compatible producer. Several consumers may connect to the same producer. There is **no general** constraint `Σ_{c ∈ Candidates(g)} x[c] ≤ 1`: multiple candidates in one group may be needed.

#### Derive execution demand from selected connections

For every compatible connection and query evaluation event `t`, linearize the conjunction of a selected connection and a running consumer:

```text
need[producer,consumer,requirement,t] ≤ connect[producer,consumer,requirement]
need[producer,consumer,requirement,t] ≤ run[consumer,t]
need[producer,consumer,requirement,t] ≥ connect[producer,consumer,requirement]
                                          + run[consumer,t] − 1
```

A candidate runs if a query root or a connected, running consumer demands it. For every candidate `c` and query evaluation event `t`:

```text
run[c,t] ≤ x[c]
run[c,t] ≥ evaluates[q,t] × r[q,c]              for each compatible query root q
run[c,t] ≥ need[c,consumer,requirement,t]       for each outgoing connection
run[c,t] ≤ Σ_q evaluates[q,t] × r[q,c]
           + Σ_{consumer,requirement} need[c,consumer,requirement,t]
```

Only defined assignments and connections appear in the sums; an empty sum is zero. `evaluates` is a fixed parameter, so these expressions are linear. Together with acyclicity, the constraints propagate demand from query roots through every required producer and prevent unsupported execution. Multiple consumers demanding the same candidate at one query evaluation event still produce a single `run[c,t] = 1`.

For example, Q1 evaluates every minute and Q2 every five minutes on the same grid. Over 60 minute ticks, assuming compatible evaluations and fresh query-time builds:

| Consumers connected to one KLL build | Build executions |
| --- | ---: |
| Q1 only | 60 |
| Q2 only | 12 |
| Q1 and Q2 | 60 |
| Neither, and no other demand | 0 |

The build's execution cost is `Σ_t execution_cost[build,t] × run[build,t]`. Additive per-execution resource usage follows the same pattern in capacity constraints. This is a joint optimization, not a circular cost definition: the solver selects connections and execution variables together, constrained by the fixed query schedule. The objective compares their resulting costs. Execution-graph cycles remain a separate issue handled by the ordering constraints.

#### Shared raw-retention configuration

For one raw-storage scope, retention candidates configure the same stored copy, so they are mutually exclusive:

```text
Σ_{c ∈ Candidates(G_raw[d])} x[c] ≤ 1
```

This is a resource-specific constraint, not a restriction on every group. For a consumer needing 15m, its compatible producers include `Retain15m` and any generated longer-retention candidates, but exclude `Retain5m`. The dependency constraints above require a selected compatible producer. A second consumer needing 5m can connect to the same `Retain15m`; storage is therefore charged once. Separate stored copies require distinct storage scopes and are charged separately.

#### Keep only connected, acyclic computation

```text
x[producer] ≤ Σ_q r[q,producer]
            + Σ_{consumer,requirement} connect[producer,consumer,requirement]
```

Only defined connections appear in these sums. Every selected candidate must serve a query output or another selected candidate. Together with acyclicity, this excludes unused computation.

Using the auxiliary `order[c]` variables defined in Section 5.1, impose the following constraint for every allowed producer–consumer connection:

```text
order[producer] + 1 ≤ order[consumer]
    + N × (1 − connect[producer,consumer,requirement])
```

When `connect = 1`, the consumer's rank must exceed the producer's by at least one. When `connect = 0`, the `N` term makes the constraint nonbinding under the rank bounds. For example, `build → merge → estimate` can use ranks `0, 1, 2`. Adding `estimate → build` would require `order[build] ≥ order[estimate] + 1`, making the cycle infeasible. Independent candidates may have equal ranks.

#### Physical implementation for every selected operation

A physical candidate must identify an executable operator for every operation it owns, including fused internal operations. Let `executable[c]` be a fixed Boolean obtained from the operator registry and deployment capabilities:

```text
x[c] ≤ executable[c]                       for every candidate c
```

Unsupported candidates can equivalently be omitted. Coverage and dependency constraints then ensure that every required operation has a selected implementation; logical expressions alone cannot be selected for execution. Only exposed outputs can supply consumers. A fused `topk(sum(...))` does not supply another query's full `sum(...)` unless that output is explicitly exposed.

#### Valid materialization and window configuration

Materialization is bundled into physical candidates. Define fixed Boolean parameters `valid_materialization[c]` and `valid_window[c]` for locally checkable legality, and `compatible[producer,consumer,requirement]` for pairwise legality:

```text
x[c] ≤ valid_materialization[c]
x[c] ≤ valid_window[c]
connect[producer,consumer,requirement]
    ≤ compatible[producer,consumer,requirement]
```

These checks include supported modes, deployment capabilities, pane alignment and coverage, retention guarantees, and producer/consumer timing. Omitting invalid candidates and connections implements the same restrictions. For example, an ingestion-maintained candidate cannot depend on data available only at query time, and a 15m consumer cannot connect to a raw-retention candidate covering only 5m.

Consumer-dependent freshness, invalidation, and lifetime checks may involve several selected connections; pairwise compatibility alone does not establish them. Such checks require a valid joint linear encoding or the feasibility cuts defined below. Retained-state candidates are eligible only with a defined refresh/update policy and its cost. The query-time `run` equations do not constitute an ingestion or retained-state model.

#### Every query meets its accuracy target

For each query `q`, the configured accuracy contract specifies an error metric and target, and, when probabilistic, a failure-probability target. A plan is feasible only if its composed output error and failure probability satisfy those targets. These are end-to-end checks, not a sum of arbitrary local sketch errors. Primitive contracts also carry their data-domain conditions. For example, an interior DDSketch quantile using interpolation cannot assume a relative-error bound across zero; a supported nonnegative-source proof can discharge that condition. Bind declared data facts before recovering dependent arithmetic guarantees, and leave unproved contracts unknown.

When proven composition rules exist, encode their bounds directly. Otherwise use the exact finite-assignment exclusion below for each plan whose validated accuracy fails. For example, if a selected P99 estimator and its small KLL producer violate the target regardless of all other choices, the following focused cut is sound:

```text
connect[small-build,estimate-P99,KLL-state] = 0
```

If upstream choices or other connections affect the error, this focused cut is insufficiently justified; exclude the complete failing assignment or a proven sufficient subset instead. Each query must pass, including those sharing the same producer. An exact operator above approximate input is not automatically exact end to end.

#### CPU, memory, and disk capacities

Resource coefficients have explicit ownership and units. For query-time recomputation, let `cpu_exec[c,t]` be local CPU-seconds per execution, `cpu_standing[c]` local background CPU-seconds over the horizon, and `cpu_fixed` nonselectable workload CPU-seconds. With `CPU_budget` measured over the same horizon:

```text
Σ_c cpu_standing[c] × x[c]
  + Σ_{c,t} cpu_exec[c,t] × run[c,t]
  + cpu_fixed ≤ CPU_budget
```

A core limit can give a CPU-time budget through available cores times horizon duration and an allowed utilization factor. This aggregate bound does not guarantee short-term throughput or deadlines; configured interval budgets and latency checks must also hold. Ingestion-maintained candidates need separately modeled update demand rather than the query-time `run` term.

For a conservative capacity model, let `resident_memory[c]` and `retained_disk[c]` bound persistent allocation, and `temporary_memory[c]` and `temporary_disk[c]` bound peak local temporary allocation per candidate under the permitted concurrency. All are bytes; fixed allocations are `memory_fixed` and `disk_fixed`:

```text
Σ_c (resident_memory[c] + temporary_memory[c]) × x[c]
    + memory_fixed ≤ Memory_capacity

Σ_c (retained_disk[c] + temporary_disk[c]) × x[c]
    + disk_fixed ≤ Disk_capacity
```

These bounds conservatively allow all selected candidates' allocations to overlap. They are safe only when the coefficients bound each candidate's concurrent executions, not merely one invocation. A phase-aware allocation model may admit more plans, but must encode actual overlap rather than substituting average occupancy. Shared state and raw retention are counted through their owning candidates once. Memory-time and disk-time used in billing are not substitutes for capacity bounds.

#### Every query meets its latency limit

For each query `q`, the configured evaluator must establish latency at or below `Latency_limit[q]` for all required query evaluation events under the declared deployment assumptions. The latency quantity must match the requirement, including any freshness or ingestion-to-visibility component. CPU-time summed over the workload is not query latency, and `order[c]` is not an execution schedule.

Where the configured latency model has a valid linear encoding, include it directly. Otherwise evaluate the selected graph and add a feasibility cut for a failing assignment. For instance, if a particular query-time build path exceeds Q1's deadline but an ingestion-maintained pane path does not, the failing path is excluded without excluding the pane choice. A focused path cut is valid only if omitted choices cannot repair its latency; otherwise use the full-assignment cut below.

#### Feasibility cuts for checks without a direct linear encoding

This rule makes deferred accuracy, latency, and joint materialization checks explicit. Let `v` be the vector of binary plan decisions: candidate selection, producer–consumer connections, root assignments, and any additional discrete policy configurations affecting the check. With a fixed schedule and acyclic graph, the query-time `run` and `need` values are derived from these decisions. For a rejected assignment `v_bar`, add:

```text
Σ_{i: v_bar[i] = 1} (1 − v[i])
  + Σ_{i: v_bar[i] = 0} v[i] ≥ 1
```

The constraint requires at least one plan decision to change. For example, rejecting `(v1,v2,v3) = (1,1,0)` gives `(1−v1) + (1−v2) + v3 ≥ 1`. The reason must be a definite failure under fixed configured models, not evaluator timeout or numerical uncertainty. If continuous settings can repair the plan, excluding only its binary assignment is not sound; include a valid continuous constraint or discretize the relevant settings.

The complete finite feasibility specification contains this inequality for every assignment violating any configured accuracy, materialization, window, resource, or latency requirement. Enumerating all such assignments is generally impractical, so a solver may add them lazily: solve, validate every requirement, exclude a failing assignment, and re-solve. Every returned plan must pass all checks. Exhaustion establishes infeasibility only for the declared finite inventory and models; timeout does not. These feasibility cuts do not repair an underestimated objective—cost refinement requires separate valid bounds.

## 6. Example: Shared KLL

For the P95/P99 workload, a selected plan can be:

```text
Input candidate → KLL build candidate
                       ├→ P95 estimator → Q1
                       └→ P99 estimator → Q2
```

Illustrative costs over the same horizon, assuming all candidates meet requirements:

| Selection | Build | Estimation | Total |
| --- | ---: | ---: | ---: |
| Local KLL computation for each query | 8 + 8 | 1 + 1 | 18 |
| One shared KLL build | 8 | 1 + 1 | 10 |

Both estimators connect to the same selected build candidate, so its cost appears once. If one configuration cannot serve all consumers economically, select multiple candidates from `G_KLL`, as in the small/large example in Section 3.

## 7. Search-Space Reduction and Validation

Candidate generation maintains a worklist of new expressions and applicable rules. Interning prevents duplicate derivations, while compatibility checks eliminate unsupported connections. Generation is relative to a finite inventory of rules, configurations, and execution scopes. If exploration stops before closure or imposes a cap, the reported optimality claim must reflect that restriction.

Let `n` be the number of physical candidates, `m` the number of compatible producer–consumer connections, and `r` the number of compatible query-output connections. The base formulation has `n + m + r` binary variables, plus ordering and any cost-model auxiliaries. This counts the model size, not its solution time. Even a restricted producer-selection case contains set cover: producers cover subsets of outputs and have activation costs. Removing interchangeable instance slots reduces redundant representation but does not remove this combinatorial difficulty.

Cost normalization must preserve small, meaningful differences relative to solver tolerances. An unused expensive candidate must not scale every feasible objective toward zero. Check the supported coefficient range and reject a claimed lower bound that exceeds an independently validated incumbent; retain the feasible plan but report the certificate as invalid.

Search reductions must preserve joint choices. A candidate that costs more alone may enable useful sharing, so local cost dominance is insufficient. Independent components can be solved separately only when dependencies, sharing, and global constraints also separate. Generation caching and sparse compatibility edges reduce overhead without requiring enumeration of complete workload plans.

Bounded fixed-calibration search first attempts to obtain a validated seed from a smaller reader-grid/GCD width domain using the same engine, then progressively adds legal widths toward the complete domain. A cached divisor frontier skips rounds that introduce no new width. Logical expansion reuses the pre-state-expansion memo and generates newly introduced window expressions; state expansion, physical inventory, and the MILP are rebuilt for each attempted domain. This is not incremental MILP solving. Only a definite budget stop may fall back to that validated seed; internal errors remain errors. Exhaustive archives and complete-catalog calibration skip this seed phase. Reports distinguish complete-domain generation from exhaustive assignment traversal, and a seed fallback supplies no full-domain optimum or gap certificate. Generation and model construction cooperate with the deadline; individual evaluator calls may overrun it.

Three complementary reductions control dense pane inventories:

- **Compatibility memoization and supplier-list sharing.** Cache exact compatibility subproblems and intern identical ordered supplier lists. Separate input positions still have separate connection decisions. This reduces repeated checking and storage, not the logical edge count or MILP variable count by itself.
- **Terminal reader factoring.** Equivalent unary summary estimators and exact-accumulator finalizers used only by query outputs can become one reader template with the union of their compatible suppliers. Equality includes operator parameters, result contracts, timing, materialization, local cost, input contracts, and output eligibility. Each output retains an independent binding through execution scopes; consumers may still choose different producers. Internal readers and pane-merge operators are excluded. In particular, merges with different pane counts are not interchangeable.
- **Progressive width generation.** Spend the shared planning budget on successively larger finite domains and retain the best validated incumbent across rounds. Report attempted domains and pending widths. A restricted-domain lower bound or gap is not a certificate for the complete domain; only a completed full-domain solve can provide that certificate.

The first two reductions preserve the represented choices under their stated contracts; the third controls work before a timeout and may leave choices unexplored. General cost-based DP compression needs a richer boundary contract covering sharing, resource use, accuracy, and execution demand; it is not implied by compatibility memoization. Fine pane widths cannot be discarded merely because their local cost is high.

Within the seed budget, the same MILP can also hold a validated plan's computations and producer connections fixed and try supported query-time materializations. The normal evaluator measures whether retaining a shared result avoids enough repeated upstream work to reduce cost. These limited proposals use the existing solve/evaluation budgets; their bounds and infeasibility apply only to the restricted proposal, never to the full search. This does not create another selector or an optimality claim for the seed.

After selection, the planner reconstructs the DAG and evaluates accuracy, latency, and resource cost. A complete MILP does not require LBBD. Lazy refinement is useful when difficult checks are left outside the initial model, provided the resulting feasibility or cost cuts are sound. The report includes the validated cost, solver status and gap, and generation limits; validating one solution alone does not prove a relaxed model optimal.

## 8. Implementation and Evaluation

The implementation uses `LogicalMemo` and `MemoGroup` for initial identities, and `CandidateMemo` / `ImplementationCandidate` for logical expansion. Local `PhysicalCandidate` choices have explicit producer requirements and compatible connections. Selection reconstructs a `PhysicalWorkloadPlan` and validates its resulting graph through the configured evaluators. The sole production engine is `CandidateConnections`; bounded and exhaustive modes use the same candidate representation and solver. Existing supported runtime operators determine which configurations can be generated.

Logical memo rules belong in `logical-asap-optimization`; physical eligibility remains authoritative in `physical-asap-optimization`. Within selection, candidate inventory/reconstruction, MILP construction, evaluator refinement, pricing support, and route orchestration have separate scopes. State-local choices belong in candidates; cross-candidate decisions use connections and constraints. The retired `CouplingSite` selector representation and legacy search routes are removed rather than retained under new names.

Evaluation must distinguish generation coverage from selection correctness. Small workloads are exhaustively enumerated over candidate subsets and compatible connections under the same inventory, then compared with MILP results. Cases include multiple selected candidates in one group, shared build costs, separate execution scopes, fusion outputs, and composed accuracy/resource requirements. Scaling experiments measure candidate and connection counts, generation and solve time, planner memory, and validated execution savings. Together these measurements establish whether the representation avoids repeated search work and whether joint selection justifies its planning cost.

### Expected planning performance and execution benefits

Planning performance and the selected plan's execution performance are separate outcomes. The following expectations are hypotheses to test, not measured speedups or solver-time guarantees.

| Workload characteristic | Expected planning performance | Expected execution benefit |
| --- | --- | --- |
| Queries have little opportunity to share. | Smaller components are easier to solve when dependencies and global constraints permit independent optimization. | Limited savings from sharing. |
| Many queries reuse a small number of states. | Memo reuse reduces repeated candidate generation; compatible connection counts still need control. | Strong opportunity to reduce repeated builds and state costs. |
| Candidates and compatible connections are dense. | MILP can become harder, especially with many possible partial-sharing configurations. | Potentially substantial savings, but proving optimality can be expensive. |
| Many window, capacity, and materialization configurations are available. | Both candidate generation and solving can grow substantially. | Benefits depend on the workload and which configurations are actually useful. |
| Accuracy and latency rely mainly on post-solve checks. | Repeated validation and refinement can make planning time highly variable. | Benefits depend on the feasible plans remaining after validation. |

### Benchmark metrics

Benchmark each workload case separately and report:

| Metric | What to measure |
| --- | --- |
| Candidate generation time | Time spent building memo groups, applying rules, and generating legal physical candidates and compatible connections. |
| Model construction time | Time spent creating MILP variables, coefficients, and constraints, including rebuilding or extending the model during refinement. |
| MILP solve time | Time inside the solver, accumulated across all refinement rounds. |
| Validation/refinement rounds | Number of plans validated and refinement rounds triggered; also report validation time separately from solve time. |
| Peak planner memory | Peak memory used during planning, including candidate generation, model construction, solver execution, and validation. |
| Validated execution cost | Cost of the returned feasible plan under the configured evaluator and common accounting horizon; distinguish this estimate from measured runtime cost. |

Report total planning wall time alongside these metrics, using consistent timing boundaries to avoid double counting.

 A richer candidate space can improve the modeled optimum while increasing planning overhead; under a fixed solver time limit, it does not guarantee a better returned plan.
