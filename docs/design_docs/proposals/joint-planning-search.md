# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal. The staged memo and selection model below includes proposed extensions.

## 1. Motivation and Challenges

ASAP primitives increase both the opportunities for query optimization and the difficulty of searching for a good plan. A quantile, for example, may be computed directly or estimated from a summary. Several quantiles may use the same summary, and several windows may use overlapping sets of panes. The cost of one query therefore depends on decisions made for other queries. Selecting the cheapest implementation of each query independently can miss the cheapest workload plan.

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

The frontend produces normalized logical expressions. The proposed memo builder interns equivalent sub-expressions and records their query roots. Consider two queries over the same input and evaluation schedule:

```text
Q1: P95(X[15m]), every 1m
Q2: P99(X[15m]), every 1m

G_input: input X
G95: P95 over X[15m]     root of Q1
G99: P99 over X[15m]     root of Q2
```

P95 and P99 remain different groups. Initial grouping before Pass 1 is proposed; current memo construction receives Pass 1 candidates.

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

A node may execute repeatedly. Its materialization, consumer schedules, and lifetime determine how frequently its work is performed and how long its state occupies resources. A constant candidate cost is appropriate only when this context is fixed. All terms use a common horizon or consistent rates, so ingestion work and evaluation work can be compared.

Some charges belong to a resource shared beyond one computation. If two consumers need 5m and 15m of the same raw source, the required retention horizon is 15m, not 20m. Such retention is charged once per source and excludes intermediate state already owned by a candidate. Configured query-request overhead is accounted for separately.

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
| Shared raw-data retention | Two consumers need 5m and 15m of the same raw source and compatible retention scope. | Charge source storage for the 15m horizon once, with reads charged according to actual work. Do not add 5m and 15m or include intermediate KLL state in this charge. |
| Per-request overhead | Two requests share computation but still require separate response handling. | Charge configured request overhead for each request. Compute sharing does not remove request-specific work. |

Memory-time and capacity answer different questions. A state occupying `m` bytes for duration `t` contributes `m × t` to memory-time. A capacity limit instead considers simultaneous residency, including temporary allocations; it cannot be inferred from memory-time alone.

The configured Stage 3 objective and evaluators remain authoritative. Current memo MILP supports usage-over-time pricing; broader cost-model support is an extension. Accuracy is an output requirement, since locally acceptable errors may compose into an unacceptable query result. Latency and resource limits likewise depend on the configured timing and capacity assumptions.

## 5. MILP Formulation

The selection problem combines supplier activation with input assignment. Ordinary bipartite matching describes compatible pairs, but does not directly capture a producer paid for once, used by several consumers, and itself dependent on other producers. A MILP expresses these decisions together. The following formulation gives the fixed-context base; demand-dependent costs require rate/lifetime variables or finite context configurations consistent with the selected graph.

### 5.1 Variables

#### Sets and notation

| Symbol | Meaning |
| --- | --- |
| `G` | Result and state groups. |
| `C_g` | Finite physical candidates belonging to group `g`. |
| `E(c)` | Required input positions of candidate `c`. |
| `S(c,e)` | Compatible supplier candidates for input `e` of candidate `c`. |
| `T` | Required query-output references. |
| `S(t)` | Candidates compatible with output reference `t`. |

Candidate IDs are globally unique; `C` is the union of all `C_g`. Input positions stay distinct even when they request the same group. For a P95 estimator, the `state` input's suppliers are compatible candidates from `G_KLL`. A query may have several output references.

```text
x[c]       ∈ {0,1}   select physical candidate c
b[c,e,d]   ∈ {0,1}   input e of c uses supplier candidate d
r[t,c]     ∈ {0,1}   query-output reference t uses candidate c
```

Connection variables exist only for compatible pairs. Capacity and materialization belong to candidate configuration, so selection needs no separate copy or instance variables.

#### Objective

```text
min Σ_c local_cost[c] × x[c]
    + shared_resource_cost
    + query_overhead
```

The first term charges each selected candidate for its local work, excluding suppliers. The remaining terms explicitly model shared resources such as source retention and configured request charges; either is zero when absent. Where selected consumers change cost, additional expressions model that dependence rather than treating it as a constant.

### 5.2 Constraints

#### Cover every query output

```text
Σ_{c ∈ S(t)} r[t,c] = 1                     for each output reference t
r[t,c] ≤ x[c]
```

#### Supply every selected candidate's inputs

```text
Σ_{d ∈ S(c,e)} b[c,e,d] = x[c]              for each required input (c,e)
b[c,e,d] ≤ x[d]
```

Several consumers may connect to the same supplier. There is **no** constraint `Σ_{c ∈ C_g} x[c] ≤ 1`: multiple candidates in one group may be needed.

#### Keep only connected, acyclic computation

```text
x[d] ≤ Σ_t r[t,d] + Σ_{c,e} b[c,e,d]
```

Only defined connections appear in these sums. Every selected candidate must serve a query output or another selected candidate. Together with acyclicity, this excludes unused computation.

If candidate generation does not already guarantee acyclicity, introduce `0 ≤ order[c] ≤ N−1`, where `N = |C|`:

```text
order[d] + 1 ≤ order[c] + N × (1 − b[c,e,d])
```

#### Enforce requirements

Candidate and connection compatibility eliminate unsupported operators, illegal materializations, and invalid window combinations. End-to-end accuracy still requires composition bounds, feasible-combination constraints, or sound feasibility refinement. Query deadlines are enforced under the configured latency model.

For additive resource usage with fixed execution context, capacity constraints take the form:

```text
Σ_c usage[k,c] × x[c] + shared_usage[k] ≤ capacity[k]
```

Peak usage and temporary allocations need lifetime and concurrency accounting when a simple sum is inappropriate. These extensions preserve the same ownership rule: suppliers and shared resources are not charged again through each consumer.

Only exposed candidate outputs can supply consumers. A fused `topk(sum(...))` candidate does not automatically supply another query's full `sum(...)`; that result needs a separate candidate unless explicitly exposed.

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

Let `n` be the number of physical candidates, `m` the number of compatible input connections, and `r` the number of compatible query-output connections. The base formulation has `n + m + r` binary variables, plus ordering and any cost-model auxiliaries. This counts the model size, not its solution time. Even a restricted supplier-selection case contains set cover: suppliers cover subsets of outputs and have activation costs. Removing interchangeable instance slots reduces redundant representation but does not remove this combinatorial difficulty.

Search reductions must preserve joint choices. A candidate that costs more alone may enable useful sharing, so local cost dominance is insufficient. Independent components can be solved separately only when dependencies, sharing, and global constraints also separate. Generation caching and sparse compatibility edges reduce overhead without requiring enumeration of complete workload plans.

After selection, the planner reconstructs the DAG and evaluates accuracy, latency, and resource cost. A complete MILP does not require LBBD. Lazy refinement is useful when difficult checks are left outside the initial model, provided the resulting feasibility or cost cuts are sound. The report includes the validated cost, solver status and gap, and generation limits; validating one solution alone does not prove a relaxed model optimal.

## 8. Implementation and Evaluation

The design extends existing memo and selection structures. `MemoGroup` stores result/state contracts, while `ImplementationCandidate` and physical generation describe supported implementations and their input requirements. Selection adds direct candidate activation and compatible connections, followed by reconstruction and pricing of the resulting graph. These are proposed extensions, not a claim that the existing implementation already supports the full representation.

Implementation proceeds from initial memo identities and rule expansion to physical configurations and connections, then joint selection and validation. State-local choices belong in candidates. Decisions genuinely spanning candidates remain explicit constraints. `SharedDecision` and `SharedDecisionKind` are the proposed names for current `CouplingSite` and `CouplingKind`; no Rust types are renamed by this document.

Evaluation must distinguish generation coverage from selection correctness. Small workloads are exhaustively enumerated over candidate subsets and compatible connections under the same inventory, then compared with MILP results. Cases include multiple selected candidates in one group, shared build costs, separate execution scopes, fusion outputs, and composed accuracy/resource requirements. Scaling experiments measure candidate and connection counts, generation and solve time, planner memory, and validated execution savings. Together these measurements establish whether the representation avoids repeated search work and whether joint selection justifies its planning cost.
