# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal. The staged memo and selection model below includes proposed extensions.

## 1. Motivation and Challenges

ASAP primitives offer more ways to compute and share query results: exact operators, summaries, panes, rollups, and scans. These choices enlarge the search space and couple query costs. A computation that is expensive for one query may be economical when shared by several queries.

The problem is to find a minimum-cost execution DAG covering every query, subject to accuracy, latency, and resource requirements, while keeping planning time and memory manageable. Shared computation and retained data must not be charged repeatedly along different query paths. Optimality is limited to the generated finite candidate set and configured models. New sketch/window algorithms and runtime scheduling are outside this proposal.

## 2. Traditional Memo Groups and Rule Expansion

A **memo group** contains expressions producing the same logical result. Expressions reference input groups rather than copying complete input plans. Rules add equivalent expressions and physical candidates; the optimizer compares their costs.

### Logical rules

Consider `Filter(A.x > 10, Join(A, B))`, where the inner join allows this filter to move onto `A`.

```text
Initially:
    G_A    = { Scan(A) }
    G_B    = { Scan(B) }
    G_join = { Join(G_A, G_B) }
    G_out  = { Filter(A.x > 10, G_join) }

After filter pushdown:
    G_filtered = { Filter(A.x > 10, G_A) }
    G_out += Join(G_filtered, G_B)
```

The new expression belongs to `G_out` because it produces the same result. The filtered input gets its own group. Further rules can expand newly added expressions; interning avoids duplicates.

### Physical rules

Implementation rules add supported physical candidates for a logical expression:

```text
Join(G_filtered, G_B)
    → HashJoin(G_filtered, G_B)
    → MergeJoin(G_filtered, G_B)
    → NestedLoopJoin(G_filtered, G_B)
```

Candidates have different costs and input requirements. For example, merge join requires ordered inputs. The optimizer typically caches `Best(group, required_properties)` and combines local operator cost with child costs.

A shared memo group avoids repeated optimization work. It does not by itself mean that execution computes the result once and shares it.

## 3. ASAPPlanner Memo Groups and Rule Expansion

ASAPPlanner uses two concepts:

- **Group:** a requested result or intermediate-state contract, including input, filters, grouping, and window semantics. State groups also specify the state interface and guarantees.
- **Candidate:** a supported way to produce that group's output. A physical candidate specifies operators, configuration, materialization, execution scope, and required inputs.

Approximate candidates target the requested result subject to accuracy requirements; they are not asserted to return exactly the same values as exact computation. **Several physical candidates in one group may be selected.** Each selected candidate becomes a node in the execution DAG. Consumers connect to specific selected candidates; sharing means using the same node.

For example:

```text
G_KLL
    C_small: small-capacity KLL ← consumers A, B
    C_large: large-capacity KLL ← consumers C, D
```

Both candidates may be selected. There is no separate physical-instance index. If two computations need separate execution scopes or incompatible input bindings, generation must provide distinct candidates that can coexist. A single selected candidate cannot have different bindings for different consumers. This is a candidate-generation requirement, not an assumption that every duplicate computation can safely be shared.

The workflow follows [planner layering](planner-layering.md). Local group candidates represent components of the workload DAGs described there.

### Stage 0: build initial groups

Parse and normalize the workload, intern equivalent logical sub-expressions, and record query roots.

```text
Q1: P95(X[15m]), every 1m
Q2: P99(X[15m]), every 1m

G_input: input X
G95: P95 over X[15m]     root of Q1
G99: P99 over X[15m]     root of Q2
```

P95 and P99 remain different groups. Initial grouping before Pass 1 is proposed; current memo construction receives Pass 1 candidates.

### Stage 1, Pass 1: expand computation candidates

Apply supported computation rules per group:

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

Enumerate executable operators, configurations, and materialization assignments. For example, maintain eligible panes at ingestion and merge them at evaluation. Retain candidate-specific compatibility and timing requirements; checks depending on actual consumers must use the selected connections.

Window semantics, algorithms, and materialization remain separate dimensions. Their definitions and support restrictions belong in [Window and Materialization Choices](window-materialization-choices.md).

### Stage 3: select and connect candidates

Select physical candidates, connect each required input to a compatible selected candidate, and cover every query root. Several consumers may share one KLL candidate or use different candidates from `G_KLL`. Reconstruct and validate the resulting `PhysicalASAPDAG`.

## 4. Cost Model

Charge each selected node for its local work and state, excluding its input nodes. A KLL build owns construction and retained state; an estimator owns extraction. Sharing a build avoids repeating its charge, while each estimator still incurs its own cost.

Costs use a common horizon or consistent rates. A selected node can execute repeatedly: materialization, evaluation schedules, and selected consumers determine its execution frequency and lifetime. A constant candidate cost is valid only when that context is fixed.

Shared raw-data retention has one owner per source. Consumers needing 5m and 15m of the same source require 15m retention, not 20m. Add configured query-request overhead separately. Do not double-count state already owned by a candidate.

Use the configured Stage 3 objective and evaluators. Current memo MILP supports usage-over-time pricing; broader cost-model support is an extension. Accuracy is checked at query outputs, and latency and resource limits must respect the configured timing model.

## 5. MILP Formulation

MILP jointly selects candidates and their connections. Ordinary bipartite matching does not capture pay-once producer activation, multiple consumers, and recursively required inputs. The formulation below is a fixed-context base; demand-dependent costs require corresponding rate/lifetime variables or finite context configurations.

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

Create connection variables only for compatible pairs. Capacity and materialization are included in candidate configuration; no separate copy or instance variables are needed.

#### Objective

```text
min Σ_c local_cost[c] × x[c]
    + shared_resource_cost
    + query_overhead
```

`local_cost` excludes suppliers. `shared_resource_cost` explicitly models resources such as per-source raw retention; `query_overhead` accounts for configured request charges. Both are zero when absent. If cost depends on selected consumers, model that dependence instead of assigning an incorrect constant.

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

- **Accuracy:** enforce valid end-to-end error bounds or sound feasibility refinement for each query.
- **Physical validity:** require supported operators, legal materialization, compatible timing, and correct window coverage.
- **Resources:** for additive fixed-context usage, constrain `Σ_c usage[k,c] × x[c] + shared_usage[k] ≤ capacity[k]`. Peak and temporary usage require appropriate lifetime/concurrency accounting.
- **Latency:** enforce deadlines under the configured evaluator's timing assumptions.

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

Intern duplicate expressions and apply rules to newly generated groups/candidates. Generate only supported configurations and execution scopes. If generation is capped or stops before rule closure, report that restriction. Removing instance indices eliminates interchangeable-slot symmetry, but does not make the optimization polynomial: even a restricted pay-once supplier selection problem contains set cover.

The base model has `|C|` selection binaries, one binary per compatible input connection, and one per compatible output connection. Reduce these counts through safe compatibility filtering, generation caching, and dominance pruning that preserves sharing opportunities. Split components only when no dependencies, sharing, or global constraints connect them.

Reconstruct the selected DAG and run accuracy, latency, and cost/resource validation. Refine and re-solve using sound cuts when necessary. MILP does not require LBBD; lazy refinement is useful when some checks cannot be encoded directly. Report validated cost, solver status, gap, and generation limits.

## 8. Implementation and Evaluation

| Phase | Deliverable |
| --- | --- |
| 1. Memo generation | Build initial identities and expand result/state groups with supported candidates. |
| 2. Physical candidates | Generate configurations, materializations, execution scopes, and compatible connections. |
| 3. Joint selection | Implement candidate selection, connection constraints, and consistent cost ownership. |
| 4. Validation | Compare tiny cases with exhaustive enumeration and measure scaling. |

Extend existing `MemoGroup`, `ImplementationCandidate`, and MILP selection structures rather than introducing a separate instance layer. `SharedDecision` and `SharedDecisionKind` remain proposed names for current `CouplingSite` and `CouplingKind`; this document does not rename Rust types. State-local configurations belong in candidates; genuinely cross-candidate decisions remain explicit constraints.

Test query coverage, multiple selected candidates per group, shared-cost accounting, scope/binding distinctions, fusion outputs, and accuracy/resource feasibility. The exhaustive reference must enumerate candidate subsets and compatible connections under the same inventory. Measure candidate/edge counts, generation and solve time, planner memory, and validated execution savings. This proposal does not claim that current code already supports the full representation.
