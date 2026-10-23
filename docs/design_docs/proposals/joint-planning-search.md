# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal

**Scope:** Multi-query optimization across exact operators, ASAP primitives, window implementations, sharing, and materialization.

## 1. Motivation and Challenges

ASAP primitives create opportunities to replace expensive exact computations and share summaries, panes, rollups, scans, or retained raw data. They also enlarge the optimization search space. Enumerating complete workload candidates multiplies implementation, sharing, window, and materialization choices. Optimizing queries independently avoids that enumeration but can miss joint savings.

The challenge is cross-query dependence: a candidate's incremental cost changes when another candidate enables a shared producer, requires a larger summary, or extends retention. Selecting the cheapest candidate for each group independently can therefore lose the workload optimum.

The design uses a memo to represent candidates compactly and a mixed-integer linear program (MILP) to select a consistent physical plan. The goals are to avoid repeated candidate generation, expose ASAP-aware sharing, and minimize the configured objective subject to accuracy, latency, and resource constraints. Optimality applies to the generated finite candidate set and configured models, not every imaginable plan. New sketch/window algorithms and runtime scheduling are outside this proposal.

## 2. Memo-Based Candidate Representation

Terminology follows [planner-layering.md](planner-layering.md):

- **Memo group:** One distinct computation and its output contract: either a query-result expression or a reusable intermediate state. Query-result identity preserves source-language semantics, inputs, filters, grouping, and windows. An intermediate-state group additionally specifies its state family/interface and the guarantees readers may rely on. Current code interns eligible identical expressions; explicitly representing shared states as groups is the proposed extension.
- **Implementation candidate:** One way to implement a group over child groups, using exact operators, exact accumulator state, or ASAP primitives. Logical candidates record dependencies and accuracy contracts; physical choices additionally specify supported operators and materialization assignments. A fused candidate can implement several groups.
- **Shared producer:** The selected implementation of a memo group read by multiple candidates, such as a KLL-state group, pane-chain group, scan group, or finer aggregation group. It uses the same group dependencies and candidate-selection mechanism as other computations, rather than a separate producer catalog.

Memo grouping avoids duplicate logical exploration. Conventional CSE exposes reuse of identical computations; ASAP-aware CSE additionally exposes shared intermediate state between different results. P95 and P99 remain separate result groups but can read one KLL. ASAP-aware CSE introduces or reuses an intermediate-state group and adds reader candidates depending on it; it does not merge different output semantics. One memo identity does not itself force runtime materialization.

In the layering document, a stage-level candidate is a DAG for the **whole workload**. Local `ImplementationCandidate` records compactly represent its component choices, avoiding construction of every complete DAG. Approximate implementations retain the original logical result as their target and carry an accuracy contract; they are not exactly equivalent expressions. Likewise, intermediate-state candidates must satisfy their group contract; different capacities or merge layouts are not automatically interchangeable. Reader compatibility constrains which state candidate can serve each reader.

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
    shared-state groups + reader candidates / windows / rollup / scans
    ↓
Stage 2: legal physical implementations and materialization assignments
    ↓
Stage 3: joint MILP selection -> reconstruct and validate PhysicalASAPDAG
```

Build initial identities after Stage 0. Intern expressions only when semantics and sharing legality match; verify equality after hashing. Preserve reader-specific requirements and keyless-node identity rules. Cache candidate generation by group and relevant context, including accuracy obligations. Current memo construction receives Pass 1 candidates; moving it earlier is a proposed change.

Pass 1 generates each computation's candidates without composing their workload-wide Cartesian product. Pass 2 matches intermediate-state signatures, introduces or reuses their groups, and adds reader candidates referencing them, including window composition. Groups can therefore be added after initial memo construction. Reusable states needed by a single candidate can also be factored out; reuse does not require two readers in advance. Stage 2 determines physical operators and materialization; Stage 3 is responsible for cost-based selection. Earlier stages prune provably invalid candidates with reasons.

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

Query results and shared intermediate states use the same group/candidate model. There are no separate producer-selection or producer-input variables. Extend the base formulation with consumer-reach, physical-copy, and lifetime variables when execution costs depend on the selected context.

### 5.1 Variables

#### Sets and notation

| Symbol | Meaning |
| --- | --- |
| `Q`, `G` | Queries and memo groups, including intermediate-state groups. |
| `C_g` | Legal physical candidates for group `g`, including state configurations and local materialization assignments. |
| `I(g,c)` | Groups implemented by candidate `(g,c)`, including fusion. |
| `R(g,c)` | Input groups required when `(g,c)` is selected, whether they produce results or intermediate state. |
| `K(g,c,h)` | Candidates of input group `h` compatible with reader candidate `(g,c)`. |

For example, a P95 estimator depends on a KLL-state group through `R`; `K` identifies that state's candidates with compatible family, window, capacity, and timing. Several required input groups create several dependencies, all of which must be satisfied.

#### Decision variables

```text
a[g]       ∈ {0,1}   group g is required
x[g,c]     ∈ {0,1}   select candidate c for group g
```

Summary capacity, pane width, retention, and materialization are candidate configurations, not a second set of producer decisions. Discrete configurations permit lookup-table costs without assuming a linear size/cost function. Materialization can instead use explicit legal assignment variables; do not maintain two representations that can disagree. Several internal nodes can require separate assignment slots.

#### Objective and cost attribution

For fixed-context costs:

```text
min  Σ_{g,c} cost[g,c] × x[g,c] + plan-level charges
```

`cost[g,c]` owns only the candidate's private work/state, down to its input-group boundaries. A KLL-state candidate owns its build and retention; an estimator candidate owns extraction. A shared merge has its own group if readers reuse its result. Never include an input group's costs again in each parent.

Selecting one state candidate does not by itself mean one execution. Materialization and consumer context determine whether it runs once per evaluation, per reader, or at ingestion. Add reach/rate and physical-copy variables for these cases; charge each actual instance correctly. Reader recurrence, phase, and range-evaluation steps affect work; a pane-alignment gcd grid is not automatically an execution rate. Raw retention and other non-computation charges remain resource/accounting variables rather than artificial computation groups.

### 5.2 Constraints

#### Query coverage and dependencies

Let `Roots(q)` contain the group references needed for query `q`, including scalar roots with multiple references:

```text
a[g] = 1                                      for g ∈ Roots(q)
Σ_{c ∈ C_g} x[g,c] = a[g]                      without fusion
```

With fused candidates, use:

```text
Σ_{(h,c): g ∈ I(h,c)} x[h,c] = a[g]
```

Every required group is implemented once, possibly by fusion; absorbed groups do not also select standalone implementations. Every selected candidate requires its inputs, including shared states:

```text
a[h] ≥ x[g,c]                                 for h ∈ R(g,c)
```

Add reverse support constraints: an active group must be justified by a root, selected consumer, or fusion coverage. This prevents unused activation even at zero prices. Dependencies are conditional on selected candidates, not every possible child. Enforce acyclicity and reuse requirements such as exact inputs or exposed state.

#### Reader/state compatibility

Requiring an input group is insufficient if only some of its candidates can serve the reader. For each input group `h`:

```text
x[g,c] ≤ Σ_{d ∈ K(g,c,h)} x[h,d]                for h ∈ R(g,c)
```

This form assumes nonfused input candidates. If another candidate implements `h` by fusion, use its compatible implementing choices in the sum instead. Compatibility covers inputs, grouping, family, reduction, windows, capacity, freshness, and timing. If several readers select the same state group, its chosen candidate must satisfy all of them.

Different pane widths can coexist as distinct intermediate groups. A group-level configuration choice must not become a global one-pane-width restriction. Retain private computation through private-state candidates/instances or the existing per-reader-copy/materialization mechanism; neither interning nor CSE should force a shared execution. Shared-instance scope is explicit where readers cannot use one configuration or lifetime.

#### Auxiliary-variable constraints

For binary `v = u AND w`, use:

```text
v ≤ u
v ≤ w
v ≥ u + w - 1
```

Use valid bounds for other products and maxima. A lower-bound-only maximum is exact only when minimization or other constraints make it tight.

#### Accuracy, validity, and budgets

- **Accuracy:** preserve state contracts and enforce proven per-query composition bounds, feasible-combination tables, or sound refinement.
- **Physical validity:** require supported operators and legal materialization assignments, including freshness, upstream timing, and state lifetime.
- **Window correctness:** require alignment, coverage without omission/double counting, retention, and algorithm-specific guarantees.
- **Resource limits:** constrain requested CPU, memory, disk, and other resources under the configured concurrency model.
- **Latency:** enforce configured query bounds and applicable recurrence checks.

For an additive resource `k` with fixed-context usage:

```text
Σ_{g,c} usage[k,g,c] × x[g,c] + plan-level usage[k] ≤ capacity[k]
```

Extend this for context-dependent rates and lifetimes. Do not use average occupancy as peak capacity or sum unrelated workloads as query latency. Resident state and concurrent temporaries may need separate constraints.

Representing sharing as group dependencies simplifies the graph but does not make costs independent. DP with complete sharing context remains possible; retaining one cheapest candidate per group can still discard the joint optimum.

## 6. Worked Example: Shared KLL for P95/P99

```text
Q1: quantile_over_time(0.95, stream[15m]), every 1m
Q2: quantile_over_time(0.99, stream[15m]), every 1m
```

The memo contains separate P95 and P99 result groups over the same eligible input. Pass 1 adds exact and KLL candidates. Pass 2 introduces a KLL-state group and reader candidates with ordinary group dependencies:

```text
G_input ──→ G_KLL: build KLL state
                /              \
       G95: estimate P95   G99: estimate P99
              |                   |
             Q1                  Q2
```

Illustrative costs per common horizon, assuming all listed candidates satisfy requirements:

| Choice | Build cost | Reader costs | Total |
| --- | ---: | ---: | ---: |
| Separate KLL builds | 8 + 8 | 1 + 1 | 18 |
| Shared KLL build | 8 | 1 + 1 | 10 |
| Separate exact computations | 12 + 12 | included | 24 |

The shared choice selects both reader candidates and one KLL-state candidate:

```text
R(G95, KLL-reader) = {G_KLL}
R(G99, KLL-reader) = {G_KLL}
R(G_KLL, build-s) = {G_input}

x[G95, KLL-reader] = x[G99, KLL-reader] = 1
x[G_KLL, build-s] = 1
```

The build cost is owned by `G_KLL` and appears once for a shared execution; each result group owns its estimation cost. If accuracy requires larger state, the solver compares compatible shared capacity, private states, and exact candidates. Quantile level alone does not determine capacity.

For ingestion-time reuse, a supported implementation maintains 1m KLL panes and merges the latest 15 at evaluation. Both readers can share panes and an eligible merge result. A direct whole-window sketch cannot be assumed to delete expired samples. Different window lengths or pane widths require separate compatibility and coverage checks.

## 7. Search-Space Reduction and Validation

Reduce search without losing valid joint choices:

1. Intern duplicate identities and cache generation with complete relevant context.
2. Remove provably invalid candidates early; apply cost dominance at Stage 3 only when external sharing interfaces and resource obligations are preserved.
3. Split coupling components only when all possible sharing, dependencies, raw retention, and constraints separate. Global capacities or calibration can prevent independent optimization.
4. Use a validated baseline incumbent and solver time/gap limits. Restricted parameter inventories narrow the optimality claim; enrichment requires solving the expanded inventory.

Reconstruct selected candidates, their group dependencies, and timing-specific copies into one `PhysicalASAPDAG`. Check dependencies and cycles, then run the configured accuracy, latency, and cost/resource evaluators. Return a validated plan, fallback, or explicit search failure status.

A complete MILP does not require LBBD. Current code uses a lower-bound model with lazy refinement for difficult costs/checks. When refinement is needed, add only sound feasibility or cost cuts conditioned on every relevant decision. Validation of one plan alone does not prove a relaxed model optimal. Report the best validated cost, certified lower bound/gap, solver status, and stop reason. Timeout without a plan is not proof of infeasibility.

## 8. Implementation and Evaluation

Reuse current structures rather than create a parallel optimizer framework:

| Structure | Responsibility |
| --- | --- |
| `MemoGroup`, `Memo` | Result/state contracts, dependencies, roots, and per-group candidates. |
| `ImplementationCandidate` | Private nodes, fusion coverage, child groups, contracts, and reuse requirements. |
| `CouplingSite` and thresholds | Existing shared-configuration representation; migrate state choices into intermediate-group candidates, retaining genuinely cross-group constraints. |
| MILP `Choice` | Physical/materialization assignments, consumer context, and coefficients. |
| `PhysicalASAPDAG`, `Selection`, reports | Selected output, validation, bounds, and search status. |

This is an extension of the current representation: shared builds are primarily coordinated through coupling sites today. Introduce intermediate-state group contracts and reader compatibility, then migrate corresponding state choices and charge ownership into group candidates. This requires changes to generation, selection, decoding, and pricing; it is not merely renaming coupling sites. Raw retention and global capacities remain accounting constraints.

Current main places memo/rules in `search/memo` and solver code in `search/milp`, with Stage 2 physical generation and Stage 3 pricing remaining authoritative. `Legacy` is the default; `MemoBruteForce` is the reference and `MemoMilp` the solver path. Early memo construction, unified shared-state groups, broader cost-model support, EH/direct sliding algorithms, and arbitrary pane hierarchies are proposed extensions.

### Implementation phases

| Phase | Deliverable and acceptance criterion |
| --- | --- |
| 1. Memo generation | Move identity construction before Pass 1 and generate candidates directly into groups without losing semantics, reader obligations, or fusion. |
| 2. Shared-state groups | Extend group contracts; generate intermediate groups and reader candidates with compatible dependencies, legal materializations, and exact-once charge ownership. |
| 3. Joint selection | Extend MILP coverage as required; match exhaustive and memo brute force under the same inventory/models. |
| 4. Validation and scaling | Validate reconstructed plans, prove safe reductions, and measure generation/model/solve costs under budgets. |

### Evaluation

**Correctness:** test identity versus sharing, producer compatibility, exact-once cost ownership, active dependencies, fusion, multiple input groups, shared/private instances, zero-price resources, pane boundaries, legal materializations, end-to-end accuracy, and capacity/status handling. Validate any relaxed bounds and cuts against enumerated plans.

**Optimality:** tiny workloads must match exhaustive enumeration and memo brute force on minimum validated cost, with an explicit numerical/tie policy and the same candidate inventory.

**Scalability:** measure groups, candidates, state configurations, dependency edges, variables/constraints, generation/solve/validation time, and peak planner memory. Compare joint versus independent planning savings, time-limited gaps, and modeled versus validated costs across query overlap, window configurations, and materializations.
