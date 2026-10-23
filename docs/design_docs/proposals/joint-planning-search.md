# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal

**Scope:** Multi-query optimization across exact operators, ASAP primitives, window implementations, sharing, and materialization.

## 1. Motivation and Challenges

ASAP primitives create opportunities to replace expensive exact computations and share summaries, panes, rollups, scans, or retained raw data. They also enlarge the optimization search space. Enumerating complete workload candidates multiplies implementation, sharing, window, and materialization choices. Optimizing queries independently avoids that enumeration but can miss joint savings.

The challenge is cross-query dependence: a candidate's incremental cost changes when another candidate enables a shared producer, requires a larger summary, or extends retention. Selecting the cheapest candidate for each group independently can therefore lose the workload optimum.

The design uses a memo to represent candidates compactly and a mixed-integer linear program (MILP) to select a consistent physical plan. The goals are to avoid repeated candidate generation, expose ASAP-aware sharing, and minimize the configured objective subject to accuracy, latency, and resource constraints. Optimality applies to the generated finite candidate set and configured models, not every imaginable plan. New sketch/window algorithms and runtime scheduling are outside this proposal.

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

ASAPPlanner follows the logical/physical stage responsibilities in [planner-layering.md](planner-layering.md), but must also represent approximate implementations and reuse between different results.

A **memo group** identifies a computation and its output contract: a query-result expression or a reusable intermediate state. A result group preserves input, filters, grouping, window semantics, and requested output. A state group additionally specifies its state family/interface and guarantees. **Implementation candidates** provide exact computation, accumulators, or summaries and reference input groups. Physical candidates add legal operators and materialization assignments. A **shared producer** is a selected group implementation read by several consumers, not a separately selected catalog entry.

Unlike exact expressions in a traditional equivalence group, approximate candidates target the same requested result under explicit accuracy obligations. State candidates must satisfy their group's contract, and readers can impose stricter compatibility requirements. One group identity does not force one runtime execution. A stage-level candidate remains a whole-workload DAG; local group candidates compactly represent its component choices.

The following example traces how the proposed memo expands:

```text
Q1: quantile_over_time(0.95, X[15m]), every 1m
Q2: quantile_over_time(0.99, X[15m]), every 1m
```

### Stage 0 and initial memo: establish logical identity

Frontends produce language-independent `LogicalDAGs`. Build groups for distinct logical sub-expressions and record query roots and reader requirements. Intern eligible identical inputs only after semantic equality and sharing legality are established; preserve keyless-node rules. P95 and P99 are different computations, so their result groups remain separate:

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

Here `s` ranges over generated capacities; each candidate records its accuracy contract and dependencies. Other computations can receive exact accumulator or fused candidates. Cache generation with relevant reader requirements and avoid composing the Cartesian product of complete workload plans.

### Stage 1, Pass 2: add intermediate groups and sharing candidates

ASAP-aware CSE recognizes that compatible KLL state supports both quantiles. Introduce or reuse a state group and add reader candidates:

```text
G_KLL: KLL state over X[15m]
    candidates: BuildKLL(G_input, window=15m, capacity=s)

G95 += EstimateQuantile(0.95, G_KLL)
G99 += EstimateQuantile(0.99, G_KLL)
```

The result groups are not merged. Their new candidates depend on the same state group, whose selected capacity must satisfy both readers. Independent candidates remain available. Conventional CSE can similarly reuse identical computations; ASAP-aware rules also expose window composition, rollup, top-k reuse, and scan widening.

For pane-based reuse, window-composition rules can add:

```text
G_panes: chain of aligned 1m KLL panes over G_input, covering 15m
G_KLL += MergeLatest15(G_panes)
```

This is another way to produce compatible window state. Other generated pane widths or compositions require coverage, merge, and accuracy checks. Groups can be added throughout rule expansion; they are not fixed after Stage 0.

### Stage 2: add physical and materialization candidates

Generate supported physical operators and assignments for these candidates. For example, an eligible `G_panes` implementation can maintain panes at ingestion while merge and estimation execute at query time. Query-time construction and supported retention modes remain candidates where legal.

Stage 2 checks deployment capabilities, ingestion eligibility, and upstream timing. It does not choose the cheapest candidate. Assignments can differ between internal nodes; sharing a logical state does not imply a shared physical execution under every assignment.

### Stage 3: select a joint physical plan

The MILP chooses candidates for both query roots and their required input groups, including any intermediate states. It may select exact/local computation, a shared whole-window build, or a legal pane-based candidate. It enforces reader/state compatibility, accuracy, latency, and resource constraints, then reconstructs and validates one `PhysicalASAPDAG`.

Shared execution is charged once, with merge and estimation charges owned by their selected groups. A higher standalone cost can enable a cheaper joint plan, so earlier stages retain valid candidates and Stage 3 performs cost-based selection.

Window semantics, implementation candidates, and materialization are defined separately in [Window and Materialization Choices](window-materialization-choices.md). That design also describes pane/EH terminology, supported configurations, and materialization eligibility.

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
- **Reader:** local merge, extraction, read, and finalization.
- **Other physical work:** local operators and temporary state.
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

`cost[g,c]` owns only the candidate's local work/state, down to its input-group boundaries. A KLL-state candidate owns its build and retention; an estimator candidate owns extraction. A shared merge has its own group if readers reuse its result. Never include an input group's costs again in each parent.

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

Different pane widths can coexist as distinct intermediate groups. A group-level configuration choice must not become a global one-pane-width restriction. Retain local computation through local-state candidates/instances or the existing per-reader-copy/materialization mechanism; neither interning nor CSE should force a shared execution. Shared-instance scope is explicit where readers cannot use one configuration or lifetime.

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

Using the groups introduced in Section 3, compare the cost of selecting shared versus local KLL candidates:

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

The build cost is owned by `G_KLL` and appears once for a shared execution; each result group owns its estimation cost. If accuracy requires larger state, the solver compares compatible shared capacity, local states, and exact candidates. Quantile level alone does not determine capacity.

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
| `ImplementationCandidate` | Local nodes, fusion coverage, child groups, contracts, and reuse requirements. |
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

**Correctness:** test identity versus sharing, producer compatibility, exact-once cost ownership, active dependencies, fusion, multiple input groups, shared/local instances, zero-price resources, pane boundaries, legal materializations, end-to-end accuracy, and capacity/status handling. Validate any relaxed bounds and cuts against enumerated plans.

**Optimality:** tiny workloads must match exhaustive enumeration and memo brute force on minimum validated cost, with an explicit numerical/tie policy and the same candidate inventory.

**Scalability:** measure groups, candidates, state configurations, dependency edges, variables/constraints, generation/solve/validation time, and peak planner memory. Compare joint versus independent planning savings, time-limited gaps, and modeled versus validated costs across query overlap, window configurations, and materializations.
