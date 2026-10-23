# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal

**Scope:** Multi-query optimization across exact operators, ASAP primitives, window implementations, sharing, and materialization.

## 1. Motivation and Challenges

ASAP primitives create opportunities to replace expensive exact computations and share summaries, panes, rollups, scans, or retained raw data. They also enlarge the optimization search space. Enumerating complete workload candidates multiplies implementation, sharing, window, and materialization choices. Optimizing queries independently avoids that enumeration but can miss joint savings.

The challenge is cross-query dependence: a candidate's incremental cost changes when another candidate enables a shared producer, requires a larger summary, or extends retention. Selecting the cheapest candidate for each group independently can therefore lose the workload optimum.

The design uses a memo to represent candidates compactly and a mixed-integer linear program (MILP) to select a consistent physical plan. The goals are to avoid repeated candidate generation, expose ASAP-aware sharing, and minimize the configured objective subject to accuracy, latency, and resource constraints. Optimality applies to the generated finite candidate set and configured models, not every imaginable plan. New sketch/window algorithms and runtime scheduling are outside this proposal.

## 2. Memo-Based Candidate Representation

Terminology follows [planner-layering.md](planner-layering.md):

- **Memo group:** One distinct logical sub-expression, preserving source-language semantics, inputs, filters, grouping, window semantics, and output contract. Proven-equivalent expressions can share an identity. Current code interns eligible identical expressions rather than discovering arbitrary equivalences.
- **Implementation candidate:** One way to implement a group over child groups, using exact operators, exact accumulator state, or ASAP primitives. Logical candidates record dependencies and accuracy contracts; physical choices additionally specify supported operators and materialization assignments. A fused candidate can implement several groups.
- **Shared producer:** A physical computation or state reusable by compatible candidates, such as a KLL build, pane chain, scan, or finer aggregation result. A producer includes its input dependencies and legal configurations.

Memo grouping avoids duplicate logical exploration. Conventional CSE exposes reuse of identical computations; ASAP-aware CSE additionally exposes shared intermediate state between different results. P95 and P99 remain separate result groups but can read one KLL. Sharing is represented between their candidates, not by merging their output semantics. One memo identity does not itself force runtime materialization.

In the layering document, a stage-level candidate is a DAG for the **whole workload**. Local `ImplementationCandidate` records compactly represent its component choices, avoiding construction of every complete DAG. Approximate implementations retain the original logical result as their target and carry an accuracy contract; they are not exactly equivalent expressions.

## 3. ASAPPlanner Optimization Workflow

```text
Stage 0: frontends -> language-independent LogicalDAGs
    ↓
Initial memo: logical identities, dependencies, query roots
    ↓
Stage 1 / Pass 1: per-computation candidates
    exact / exact accumulator / summary family and size / fusion
    ↓
Stage 1 / Pass 2: ASAP-aware CSE candidates
    shared builds / window composition / rollup / top-k reuse / scans
    ↓
Stage 2: legal physical implementations and materialization assignments
    ↓
Stage 3: joint MILP selection -> reconstruct and validate PhysicalASAPDAG
```

Build initial identities after Stage 0. Intern expressions only when semantics and sharing legality match; verify equality after hashing. Preserve reader-specific requirements and keyless-node identity rules. Cache candidate generation by group and relevant context, including accuracy obligations. Current memo construction receives Pass 1 candidates; moving it earlier is a proposed change.

Pass 1 generates each computation's candidates without composing their workload-wide Cartesian product. Pass 2 matches producer signatures and adds compatible sharing candidates, including window composition. Stage 2 determines physical operators and materialization; Stage 3 is responsible for cost-based selection. Earlier stages prune provably invalid candidates with reasons.

### Window and materialization choices

Keep these dimensions separate:

- **Semantics:** requested window width, offset, evaluation timestamps, and alignment.
- **Implementation:** raw recomputation, supported direct sliding state, fixed-width panes, or an EH algorithm.
- **Materialization:** when each internal state is computed and how long it is retained.

A pane is an aligned, non-overlapping fixed-width interval and its aggregate/summary state. An EH bucket represents a portion of the stream under an exponential histogram algorithm, with algorithm-specific merge, expiration, and error rules.

| Implementation | Ingestion, when maintained there | Evaluation |
| --- | --- | --- |
| Direct sliding-window state | Add data and remove expired contributions. | Read state and compute result. |
| Pane-based | Update panes and evict expired panes. | Merge required panes and compute result. |
| EH-based | Update, merge, and evict buckets. | Estimate the window result. |

This table describes possible strategies. Current pane support does not imply general direct sliding-state or EH support. Offer only executable candidates. Multiple pane widths can coexist; alignment, coverage, mergeability, retention, and error contracts determine legality. A 15m window evaluated every 1m can use 1m panes; complete 5m panes alone generally cannot cover every endpoint.

Use existing `Materialization` options: `NotMaterialized`, `MatAtQuery(Memory)`, `MatAtIngestion(Memory)`, and `MatAtQueryKeptAcrossEvaluations(Memory)`. Assign them to eligible internal nodes, not necessarily the entire query. For example, maintain panes at ingestion and merge at query time. Generate only supported assignments: retained query-time state currently requires deployment capability and an eligible pane chain. Ingestion timing and upstream dependencies must satisfy Stage 2 rules.

## 4. Cost Model and Optimization Objective

Attribute resource usage to physical nodes in the selected DAG, then apply the existing Stage 3 cost model:

| Model | Selection policy |
| --- | --- |
| `UsageOverTime(ResourceUnitCost)` | Minimize priced usage per second. |
| `ProvisionedCapacity(ResourceUnitCost)` | Minimize priced provisioned memory, cores, and disk. |
| `ClusterCapacity(ClusterLimits)` | Enforce capacities and use `LowestLatency`, `LeastMemory`, or `ParetoFront`. |

Current memo MILP supports usage-over-time pricing; unsupported models route to legacy search. Broader MILP coverage is an extension. A Pareto objective requires frontier computation rather than one scalar solve.

Give every charge one owner:

- **Producer:** build/update, maintenance, and retained state.
- **Reader:** private merge, extraction, read, and finalization.
- **Other physical work:** private operators and temporary state.
- **Shared accounting:** per-source raw retention and other plan-level charges.

Charge shared work once per actual execution or maintained state, not once per query path. Reader-specific work remains separately charged. Use a common horizon or consistent per-second rates; distinguish memory/storage occupancy over time from peak capacity. Producer costs can depend on selected readers, evaluation schedules, upstream materialization, and retention. Model that context rather than assuming every producer has a constant cost.

Accuracy is an end-to-end root requirement. Compose summary and window error only with valid contracts; locally admissible sketches do not prove a composed query accurate. Latency and resource limits must use the configured evaluator's timing and concurrency assumptions. Current latency accounting sums reached query-time work plus applicable compaction; this proposal does not silently replace it with a general parallel scheduler. Keep calibrated model inputs fixed during a solve.

## 5. MILP Formulation

The base formulation below separates logical coverage, shared producers, and reader attachments. Extend it with consumer-reach variables and context-dependent charges where necessary, as current code does.

### 5.1 Sets and variables

| Symbol | Meaning |
| --- | --- |
| `Q`, `G` | Queries and memo groups. |
| `C_g` | Legal physical candidates owned by group `g`, including local materialization assignments. |
| `B`, `S_b` | Potential producer instances and their finite configurations. |
| `I(g,c)` | Groups implemented by candidate `(g,c)`, including fusion. |
| `R(g,c)` | Input groups required when `(g,c)` is selected. |
| `J(g,c)` | Producer inputs required by `(g,c)`. |
| `A(g,c,j)` | Compatible producer/configuration pairs for producer input `j`. |

A producer input is an input slot, such as “pane chain”; it is filled by one compatible producer/configuration. `R` references logical results, while `J` references separately represented shared state. Do not encode the same dependency twice. If a producer is an ordinary memo group, its dependency can instead use `R`.

```text
a[g]             ∈ {0,1}   group g is required
x[g,c]           ∈ {0,1}   select candidate c for g
y[b,s]           ∈ {0,1}   enable producer b at configuration s
z[g,c,j,b,s]     ∈ {0,1}   connect producer input j of (g,c) to (b,s)
```

Configuration `s` can include summary capacity, pane width, retention, and materialization. Discrete configurations allow measured lookup-table costs without assuming a linear size/cost function. Materialization can instead use explicit legal `x[g,c,m]` variables; avoid conflicting representations. Several internal nodes can require separate assignment slots.

### 5.2 Query coverage and dependencies

Let `Roots(q)` contain the group references needed for query `q`, including scalar roots with multiple references:

```text
a[g] = 1                                      for g ∈ Roots(q)
Σ_{c ∈ C_g} x[g,c] = a[g]                      without fusion
```

With fused candidates, use the general coverage constraint:

```text
Σ_{(h,c): g ∈ I(h,c)} x[h,c] = a[g]
```

Every required group is implemented once, possibly by fusion; absorbed groups do not also select standalone implementations. Selected candidates require their inputs:

```text
a[h] ≥ x[g,c]                                 for h ∈ R(g,c)
```

Add reverse support constraints: an active group must be justified by a root, selected consuming candidate/producer, or fusion coverage. This prevents unused activation even at zero prices. Dependencies are conditional on selected candidates, not every possible child. Enforce acyclicity, exact-input requirements, and exposed-state requirements for reuse.

### 5.3 Shared producers and compatibility

For each required producer input:

```text
Σ_{(b,s) ∈ A(g,c,j)} z[g,c,j,b,s] = x[g,c]       for j ∈ J(g,c)
z[g,c,j,b,s] ≤ y[b,s]
```

Use one equality per producer input: a candidate needing a scan and a pane chain needs both inputs. Local execution is a separate candidate or explicit fallback choice.

For one configurable producer instance:

```text
Σ_{s ∈ S_b} y[b,s] ≤ 1
y[b,s] ≤ Σ selected attachments requiring (b,s)
```

Represent coexisting configurations as separate instances or explicitly permit them. This is not a global restriction to one pane width. Attachments include downstream producers; encode producers' upstream dependencies too. Compatibility covers inputs, grouping, family, reduction, windows, capacity, freshness, and timing.

### 5.4 Objective and cost attribution

For fixed-context coefficients:

```text
min  Σ_{g,c} d[g,c] × x[g,c]
   + Σ_{b,s} f[b,s] × y[b,s]
   + Σ_{g,c,j,b,s} r[g,c,j,b,s] × z[g,c,j,b,s]
```

Here `d` is private incremental cost, `f` owns producer build/update/retention costs, and `r` is reader-specific work. All use the same horizon. Exclude shared costs from `d` to avoid duplicate charges; reuse of a merge result also needs explicit ownership.

When execution rates depend on selected readers or ancestors, add consumer-reach/rate variables rather than using unconditional `f` or `r`. Reader recurrence, phase, and range-evaluation steps affect actual work; a pane-alignment gcd grid is not automatically an execution rate. Raw and pane retention depend on the longest required horizons.

For binary `v = u AND w`, use:

```text
v ≤ u
v ≤ w
v ≥ u + w - 1
```

Use valid bounds for other products and maxima. A lower-bound-only maximum is exact only when minimization or other constraints make it tight.

### 5.5 Accuracy, validity, and budgets

- **Accuracy:** filter incompatible configurations and enforce proven per-query composition bounds, feasible-combination tables, or sound refinement.
- **Physical validity:** require supported operators and legal materialization assignments, including freshness, upstream timing, and state lifetime.
- **Window correctness:** require alignment, coverage without omission/double counting, retention, and algorithm-specific guarantees.
- **Resource limits:** constrain requested CPU, memory, disk, and other resources under the configured concurrency model.
- **Latency:** enforce configured query bounds and applicable recurrence checks.

For an additive resource `k`:

```text
Σ u_x[k,g,c] × x[g,c] + Σ u_y[k,b,s] × y[b,s]
  + Σ u_z[k,g,c,j,b,s] × z[g,c,j,b,s] ≤ U[k]
```

Do not use average occupancy as peak capacity or sum unrelated workloads as query latency. Resident state and concurrent temporaries may need separate constraints.

A DP retaining one cheapest candidate per group can discard sharing benefits. DP with full sharing context remains possible, but its state can grow combinatorially; MILP handles these discrete choices jointly.

## 6. Worked Example: Shared KLL for P95/P99

```text
Q1: quantile_over_time(0.95, stream[15m]), every 1m
Q2: quantile_over_time(0.99, stream[15m]), every 1m
```

The memo contains separate P95 and P99 result groups over the same eligible input. Pass 1 adds exact and KLL candidates. Pass 2 identifies one compatible shared KLL build with separate estimation nodes:

```text
                   Shared KLL producer
                      /         \
                extract P95   extract P99
                     |             |
                    Q1            Q2
```

Illustrative costs per common horizon, assuming all listed candidates satisfy requirements:

| Choice | Build cost | Reader costs | Total |
| --- | ---: | ---: | ---: |
| Separate KLL builds | 8 + 8 | 1 + 1 | 18 |
| Shared KLL build | 8 | 1 + 1 | 10 |
| Separate exact computations | 12 + 12 | included | 24 |

The shared choice selects both reader candidates and attachments but one producer configuration:

```text
x[G95, KLL-reader] = x[G99, KLL-reader] = 1
y[Shared-KLL, s] = 1
z[G95, KLL-reader, summary, Shared-KLL, s] = 1
z[G99, KLL-reader, summary, Shared-KLL, s] = 1
```

If accuracy requires larger state, the solver compares compatible shared capacity, private states, and exact candidates. Quantile level alone does not determine capacity.

For ingestion-time reuse, a supported implementation maintains 1m KLL panes and merges the latest 15 at evaluation. Both readers can share panes and an eligible merge result. A direct whole-window sketch cannot be assumed to delete expired samples. Different window lengths or pane widths require separate compatibility and coverage checks.

## 7. Search-Space Reduction and Validation

Reduce search without losing valid joint choices:

1. Intern duplicate identities and cache generation with complete relevant context.
2. Remove provably invalid candidates early; apply cost dominance at Stage 3 only when external sharing interfaces and resource obligations are preserved.
3. Split coupling components only when all possible sharing, dependencies, raw retention, and constraints separate. Global capacities or calibration can prevent independent optimization.
4. Use a validated baseline incumbent and solver time/gap limits. Restricted parameter inventories narrow the optimality claim; enrichment requires solving the expanded inventory.

Reconstruct selected candidates, producer attachments, and timing-specific copies into one `PhysicalASAPDAG`. Check dependencies and cycles, then run the configured accuracy, latency, and cost/resource evaluators. Return a validated plan, fallback, or explicit search failure status.

A complete MILP does not require LBBD. Current code uses a lower-bound model with lazy refinement for difficult costs/checks. When refinement is needed, add only sound feasibility or cost cuts conditioned on every relevant decision. Validation of one plan alone does not prove a relaxed model optimal. Report the best validated cost, certified lower bound/gap, solver status, and stop reason. Timeout without a plan is not proof of infeasibility.

## 8. Implementation and Evaluation

Reuse current structures rather than create a parallel optimizer framework:

| Structure | Responsibility |
| --- | --- |
| `MemoGroup`, `Memo` | Logical identity, dependencies, roots, and per-group candidates. |
| `ImplementationCandidate` | Private nodes, fusion coverage, child groups, contracts, and reuse requirements. |
| `CouplingSite` and thresholds | Shared configurations and compatible readers. |
| MILP `Choice` | Physical/materialization assignments, consumer context, and coefficients. |
| `PhysicalASAPDAG`, `Selection`, reports | Selected output, validation, bounds, and search status. |

Shared-producer and producer-input notation maps onto existing sites and shared-node identities; it does not imply a separate catalog already exists. Add explicit records only where the existing representation cannot express ownership or dependencies.

Current main places memo/rules in `search/memo` and solver code in `search/milp`, with Stage 2 physical generation and Stage 3 pricing remaining authoritative. `Legacy` is the default; `MemoBruteForce` is the reference and `MemoMilp` the solver path. Early memo construction, broader cost-model support, EH/direct sliding algorithms, and arbitrary pane hierarchies are proposed extensions.

### Implementation phases

| Phase | Deliverable and acceptance criterion |
| --- | --- |
| 1. Memo generation | Move identity construction before Pass 1 and generate candidates directly into groups without losing semantics, reader obligations, or fusion. |
| 2. Sharing and physical choices | Reuse sites and materialization rules; represent producers, producer inputs, and shared charges consistently. |
| 3. Joint selection | Extend MILP coverage as required; match exhaustive and memo brute force under the same inventory/models. |
| 4. Validation and scaling | Validate reconstructed plans, prove safe reductions, and measure generation/model/solve costs under budgets. |

### Evaluation

**Correctness:** test identity versus sharing, producer compatibility, exact-once cost ownership, active dependencies, fusion, multiple producer inputs, zero-price resources, pane boundaries, legal materializations, end-to-end accuracy, and capacity/status handling. Validate any relaxed bounds and cuts against enumerated plans.

**Optimality:** tiny workloads must match exhaustive enumeration and memo brute force on minimum validated cost, with an explicit numerical/tie policy and the same candidate inventory.

**Scalability:** measure groups, candidates, configurations, attachments, variables/constraints, generation/solve/validation time, and peak planner memory. Compare joint versus independent planning savings, time-limited gaps, and modeled versus validated costs across query overlap, window configurations, and materializations.
