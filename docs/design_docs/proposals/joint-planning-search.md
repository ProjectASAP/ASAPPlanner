# Memo group and MILP design

Status: design proposal. Audience: planner designers and implementers.

ASAP primitives introduce sharing opportunities across queries, but also enlarge
ASAPPlanner's optimization search space. The optimizer must represent these
opportunities compactly and select a low-cost, valid physical plan for the whole
workload. This design uses memo groups for candidate generation and a mixed-integer
linear program (MILP) for joint selection. It preserves the responsibilities of
the stages in [planner-layering.md](planner-layering.md).

## Challenges and goals

Whole-workload enumeration multiplies per-computation candidates, sharing
configurations, and materialization assignments. The earlier joint-search
measurements on a growing prefix of one random mixed workload illustrate this
problem:

| Queries | Planning time | Peak memory |
| ---: | ---: | ---: |
| 3 | 0.4 s | 0.14 GB |
| 4 | 5 s | 1.5 GB |
| 5 | 129 s | 24.7 GB |
| 6 | stopped at about 228 s | about 100 GB |

These are historical measurements, not a benchmark of the proposed design.

The goals are to reduce duplicate candidate generation, avoid enumerating the
Cartesian product of whole-workload plans, and preserve the minimum cost over the
configured finite candidate space. A time-limited search must return its best
validated plan and report whether optimality is proved. Incremental replanning
and new window algorithms are separate work.

## Memo groups and candidate generation

A memo group identifies one logical computation and its output contract. Exact
candidates produce the same logical result. Approximate candidates implement that
result subject to explicit accuracy obligations; they are not exactly equivalent
expressions. A group is not a sketch family or a collection of all computations
that could share a sketch.

Build the initial memo after Stage 0, before Stage 1 Pass 1:

- Create a group for each distinct logical sub-expression.
- Merge identical expressions only where semantic identity and sharing legality
  are established; verify equality after hashing. Preserve schema, grouping,
  source-language semantics, and time semantics. Keyless nodes can require
  separate identities per reader.
- Record group dependencies, query roots, and reader requirements.

Pass 1 generates candidates directly against these group identities. Cache
candidate generation by group and all relevant context, including accuracy
requirements. Different readers may need different summary sizes; merging their
logical identity must not discard candidates needed by a reader. This avoids
repeating generation for identical computations without first composing every
whole-query plan.

### Memo grouping versus CSE

Memo grouping represents the search space. Common subexpression elimination
(CSE) identifies computations that can reuse work. A repeated identical
expression can have one memo group with multiple parents, but that identity alone
does not require one materialized execution at runtime.

ASAP-aware CSE also identifies sharing between different logical computations:
summary capability sharing, pane composition, rollup, top-k reuse, and scan
widening. These rules add candidates and dependencies between groups; they do not
merge groups whose requested results differ.

For example, p95 and p99 over the same input window are separate result groups.
Their KLL candidates may share one build, with separate estimation nodes. A
coupling site identifies the shared configuration decisions, such as build size
or a pane chain's grid and retention span. The solver must select compatible
configurations for all attached readers. A shared build can instead be represented
as an explicit intermediate group; either representation must preserve the same
choices and charge ownership.

### Responsibilities of the stages

| Stage | Decisions represented in the memo |
| --- | --- |
| Stage 0 | Parse and lower queries into logical DAGs; establish query semantics. |
| Initial memo construction | Establish logical group identities, dependencies, and query roots. |
| Stage 1, Pass 1 | Add exact, exact-accumulator, summary-family, summary-size, and fused-computation candidates. |
| Stage 1, Pass 2 | Add ASAP-aware CSE candidates, shared configurations, and cross-group dependencies. |
| Stage 2 | Generate legal physical implementation and materialization candidates, including assignments to internal nodes. |
| Stage 3 | Jointly select candidates under the configured objective and constraints; validate the decoded plan. |

Candidate generation can be lazy. Only provably invalid or contextually dominated
candidates may be removed without changing the finite-space optimum. A higher
standalone cost is not enough to discard a candidate that enables sharing.

## Cost-based joint optimization

Assign resource usage to the nodes of the selected physical DAG using the
configured cost model. The objective can minimize billed cost, including CPU,
memory, storage, and other priced resources. In a cluster without billing prices,
select an explicit objective, such as latency or memory, subject to capacity
limits. Capacity limits alone do not define an optimum.

A shared computation is charged once per actual execution, not once per reader.
An ingestion-maintained build has update and retained-state costs; query-time
work has costs at its actual trigger rate. Multiple consumers can still cause
multiple executions of an unmaterialized node. Estimation and merge work private
to each reader is charged separately.

### Why joint selection is needed

Consider two result groups, a quantile at 0.5 and a quantile at 0.9, over the same
input window. Assume their private KLL implementations cost 4 and 6 units per
second. A shared build satisfying both readers costs 7, with each estimator
costing 0.1. All configurations in this example meet accuracy requirements.

Optimizing the groups independently selects the private implementations for a
combined cost of 10. Joint selection can choose the shared build for 7.2. The
incremental cost of attaching a reader depends on whether another selected
candidate already enables the build. It can also depend on whether that reader
requires a larger build, a longer retention horizon, or more frequent evaluation.

A dynamic program retaining only the cheapest candidate per group loses this
context. DP is possible if its state includes the relevant reader sets, shared
configurations, materializations, and resource tradeoffs, but those states can
grow combinatorially. MILP provides a general engine for these joint discrete
choices. Local DP and caching remain useful where dependencies are limited.

## Window model

Distinguish three dimensions:

1. **Window semantics:** width, evaluation step, time origin/alignment, and endpoint
   conventions. A 15m trailing window evaluated every 1m is a sliding-window query.
   With width and step both 15m and aligned boundaries, windows are tumbling.
2. **Window implementation:** how the required data is represented and combined,
   such as raw recomputation, direct sliding-state maintenance, fixed-width panes,
   or exponential histogram (EH) buckets.
3. **Materialization:** when each internal state is computed and how long it is
   retained. Sliding-window semantics do not prevent ingestion-time maintenance.

### Panes and EH buckets

**Panes** partition a timeline into aligned, non-overlapping intervals of a fixed
width. Each pane contains data or aggregate/summary state for its interval. A
window combines the required panes, with explicit boundary handling when its
endpoints do not align. An implementation may maintain several pane widths or a
hierarchy where larger panes are formed from smaller ones.

**EH buckets** follow an exponential histogram algorithm: eligible buckets are
merged into geometrically increasing size levels and expired buckets are
removed. Standard EH sizes commonly measure represented item counts, rather than
fixed time durations. The algorithm must define the statistic it supports and
bound error from partially overlapping the oldest bucket. EH cannot be assumed
to work with every summary family, including an arbitrary KLL composition.

Pane width and EH bucket size are separate from the summary-size parameters
controlling the state within each pane or bucket.

### Window configurations

For a 15m window, candidate pane compositions might include 15 × 1m, 5 × 3m,
3 × 5m, or a hierarchy combining fine boundary panes and coarser interior panes.
These are candidates only when they match the query's evaluation boundaries or
include a valid boundary treatment. For a 15m window evaluated every 1m, complete
5m panes alone cannot generally represent every evaluation exactly.

The solver may enable multiple pane widths simultaneously. It chooses which
states to maintain, whether they are built from raw input or finer panes, and
which composition each query reads. Do not impose a global constraint selecting
only one pane width. Shared configuration sites must distinguish incompatible
chains rather than collapse every pane computation into one site.

| Implementation | Ingestion-time work, if maintained there | Evaluation work |
| --- | --- | --- |
| Direct sliding state | Insert new data and expire old contributions, where the statistic supports it. | Read/finalize current state. |
| Pane-based | Update the active pane and maintain retention. | Read, merge, and estimate from the required panes, unless those results are also maintained. |
| EH-based | Update, merge, and expire buckets according to the supported algorithm. | Apply its window estimator. |

This table describes design options, not an implementation-completeness claim.
The inspected memo implementation provides tumbling-pane and pane-chain
candidates and summary computation support. It does not establish a general
direct sliding-state implementation or EH support. Pane input binding,
scheduling, and retention also require deployment support. Unsupported strategies
must not be offered as executable candidates.

## MILP formulation

The following is a design-level formulation. The implementation may expand a
choice into several internal-node decisions rather than enumerate all local
assignments. It must not enumerate the product of complete workload plans.

### Choices and variables

Let `G` be the logical groups and `C(g)` the legal physical choices for group `g`.
A choice includes its implementation, window configuration, private-node summary
parameters, and legal materialization assignment. Shared configurations are
represented separately so their costs are not duplicated.

```text
n[g]   ∈ {0,1}    logical group g is required
x[g,c] ∈ {0,1}    select physical choice c for group g
y[b,t] ∈ {0,1}    enable shared computation instance b at configuration t
```

A shared configuration `t` can include summary size `s`, pane width, retention,
and materialization. Thus `y[b,s]` is the special case where only summary size
varies. Materialization can also be explicit as `x[g,c,m]`; when a candidate has
several internal nodes, `m` denotes an assignment to those nodes, not one label
for the entire query.

The materialization vocabulary is:

| Option | Meaning |
| --- | --- |
| `NotMaterialized` | Compute through a consumer without retaining an independently reusable result. |
| `MatAtQuery` | Build at query time and reuse within the applicable evaluation. |
| `MatAtIngestion` | Maintain state as input arrives and retain it for readers. |
| `MatAtQueryKeptAcrossEvaluations` | Build at query time and retain across evaluations, with valid freshness and maintenance behavior. |

Generate only legal assignments. A reader can use ingestion-maintained panes
while performing merge and estimation at query time.

Enumerate a finite set of supported size/configuration values and derive costs
for each. The mapping from size to cost need not be linear: a precomputed cost
is a constant coefficient multiplying a binary configuration variable. This
approach is exact only over the generated configurations; exhaustive configuration
enumeration can itself become expensive.

### Coverage and dependencies

Each query root must be required:

```text
n[root(q)] = 1
```

Let `implements(h,c)` contain all groups implemented by choice `(h,c)`, including
groups absorbed by fusion. Require each needed group to be covered once:

```text
Σ_{(h,c): g ∈ implements(h,c)} x[h,c] = n[g]
```

If choice `(h,c)` reads logical group `d`, it requires that group:

```text
x[h,c] ≤ n[d]
```

Candidates must also satisfy exact-input, state-exposure, and other reuse
requirements. Dependencies must be acyclic, either by candidate construction or
by explicit ordering constraints. Sharing a result and computing separate
physical copies may be distinct choices while preserving one logical identity.

### Shared producers and compatibility

For each reader choice using shared instance `b`, let `A(g,c,b)` be the compatible
configurations:

```text
x[g,c] ≤ Σ_{t ∈ A(g,c,b)} y[b,t]
Σ_t y[b,t] ≤ 1                       for one shared instance b
```

Compatibility includes input, family, grouping, window boundaries, summary size,
and materialization. If multiple configurations must coexist, represent separate
instances or an explicit multi-configuration structure. Each enabled producer
must have its own input dependencies and legal production implementation.

Where activation means a producer serves at least one reader, add:

```text
y[b,t] ≤ Σ_{(g,c) able to read (b,t)} x[g,c]
```

Do not rely solely on a positive objective coefficient to suppress unused
producers; zero-price resources are possible.

### Accuracy, windows, and physical validity

- Every query's composed guarantee must satisfy its target, including applicable
  confidence requirements. Summary error and window-representation error require
  a valid composition rule, not an assumed arithmetic sum.
- Every window read must cover the correct data at each modeled evaluation,
  without omission or double counting. Regular-grid checks can establish this
  symbolically without expanding every timestamp.
- Every selected operation must have a supported physical implementation.
- A selected materialization must provide fresh, available state when its readers
  execute. Enforce the actual physical compatibility rules rather than assuming
  that every ancestor forces the same materialization on all descendants.
- Raw and intermediate-state retention must cover every selected reader's needs.

Reject provably invalid choices during generation. Encode remaining joint checks
in the model, or use a sound validation/refinement mechanism described below.

### Consumer rates and resource accounting

Introduce consumer-reach and activation variables where a node's execution rate
depends on selected ancestors, readers, and materialization. For a binary product
`v = a AND b`, use the complete linearization:

```text
v ≤ a
v ≤ b
v ≥ a + b - 1
```

A query-time shared build runs at the union of actual reader triggers, accounting
for time alignment and reuse scope. Two readers on 60s and 90s schedules aligned
at zero have union rate `1/60 + 1/90 - 1/180`, not the rate of the 30s gcd grid.
A gcd grid may describe legal pane alignment without describing execution count.

For one raw source, retention follows the longest required raw horizon under
the selected storage policy, rather than the sum of per-query copies. Pane
retention similarly follows the selected readers and maintained layers. If a
maximum is modeled only with lower bounds, positivity/minimization may make it
tight; otherwise add constraints enforcing its intended value.

### Objective and optional capacity constraints

The billing objective is:

```text
min  selected private-node execution costs
   + enabled shared-producer execution costs
   + retained raw and intermediate-state costs
   + temporary-state and other plan-level costs
```

Give each charge one owner. A shared build must not be included both in every
reader's private coefficient and in `y[b,t]`. Account for update, merge,
estimation, expiration, and compaction where the selected implementation needs
them. Freeze calibration and model inputs before solving.

If requested, constrain CPU, memory, disk, and per-query latency. Resource
coefficients must match their meaning: average memory is not peak memory, and
summing query work does not establish concurrent CPU or memory demand without
an execution/concurrency model. A cluster objective can minimize latency or
memory under these constraints. A Pareto request requires a frontier search,
not a claim that one scalar solve returns every nondominated plan.

## Solving and validation

Start with a complete MILP for the supported finite candidate space where the
costs and constraints can be encoded accurately and compactly. MILP does not
require logic-based Benders decomposition (LBBD).

A separate exact evaluator remains useful for checking a decoded plan. Validation
alone cannot certify optimality if the model omits costs or legal candidates.
If difficult composed checks or costs are deliberately omitted, the model must
be a valid relaxation, and refinement is required:

1. Solve the relaxed model and retain its certified solver lower bound.
2. Decode a complete physical plan and validate its accuracy, timing, resources,
   and full cost. A validated plan supplies an incumbent upper bound.
3. Exclude a proved-infeasible combination, or add a proved-valid cost bound.
4. Repeat until the certified bound closes the target gap or the budget expires.

A no-good cut must include every decision needed to prove infeasibility. A cost
cut must be conditioned on all choices affecting that charge; applying a
whole-plan cost to an incomplete candidate signature can exclude a cheaper plan.
Excluding a feasible, evaluated configuration is safe only if its validated plan
is retained and the proof accounts for that excluded region. Do not confuse a
solver's current feasible objective with its certified global lower bound.

LBBD is one possible master/subproblem design for this refinement, not a
requirement. An iterative cut loop is not automatically LBBD. Select a
decomposition only when measured model size or difficult checks justify it.

Split workload components only when all potential sharing, retention, and
feasibility interactions are absent. Global capacity constraints can couple
otherwise separate components: keep the joint model or combine complete resource
frontiers. Unknown interactions remain connected.

### Reporting and exactness

Report candidate/configuration counts, model size, generation and solver time,
validation effort, incumbent cost, certified lower bound, gap, and stop reason.
The gap is `(U-L)/U` for positive incumbent cost `U`; define zero-cost handling
explicitly. A time limit with no incumbent means no feasible plan found, not
proved infeasible. If refinement has exhausted the remaining region but retained
an incumbent, return that incumbent rather than reporting the workload infeasible.

Optimality is relative to the generated finite candidates and configured models.
It is not a guarantee of measured runtime, completeness over all pane widths or
summary sizes, or support for unimplemented algorithms. Ties require an explicit
selection policy; equal-cost plans need not have identical structure.

## Workflow and example

```text
Stage 0: queries → logical DAGs
    ↓
Build initial memo: identities, dependencies, query roots
    ↓
Stage 1, Pass 1: generate per-group computation candidates
    ↓
Stage 1, Pass 2: add ASAP-aware CSE candidates and shared configurations
    ↓
Stage 2: generate legal physical and materialization candidates
    ↓
Stage 3: build and solve the joint MILP
    ↓
Decode and validate the selected physical DAG
    ↓
Return a validated plan and search report; refine the model if required
```

Consider:

```text
Q1: p95(latency[15m]), evaluated every 1m
Q2: p99(latency[15m]), evaluated every 1m
```

Stage 0 establishes two different quantile results with the same window width
and evaluation step. Initial memo construction keeps separate result groups and
shares eligible identical inputs. Pass 1 adds exact and supported KLL candidates,
with sizes derived from the readers' accuracy requirements.

Pass 2 adds candidates for a shared whole-window KLL build and supported pane
compositions. One possible configuration maintains 1m KLL panes and reads the
latest 15 panes. The two estimates can share the build, and, where legal, a merge
result. Other widths and hierarchies remain candidates only when generated and
supported; 5m panes alone cannot cover every 1m-aligned evaluation.

Stage 2 supplies legal assignments: for example, pane builds at ingestion and
merge/estimate at query time, or query-time construction from retained raw data.
Stage 3 compares their full costs. A shared build may be sized for both readers;
private builds remain candidates. The selected plan meets both accuracy targets,
window semantics, latency limits, and any configured resource capacities.

## Implementation and verification

The inspected memo implementation in
`crates/plan-selection/src/pass/subexpr/` separates group construction,
summary capability sharing, window composition, scan sharing, pricing, and search.
Its memo builder currently receives Pass 1 candidates. Moving group construction
after Stage 0 and adapting Pass 1 to generate directly into groups is the target
architecture, not an assertion about the current call order.

Likewise, the inspected MILP path supports usage-over-time pricing and routes
unsupported cost models to legacy search. Cluster-capacity and provisioned-cost
formulations in this document are design requirements, not claims of current
MILP coverage. EH, general direct sliding-state maintenance, and arbitrary
multi-layer pane generation require their own implementation work.

Verify the design against exhaustive enumeration on small workloads and a
separate brute-force search over the memo. Compare minimum validated cost under
the same candidate inventory and models, not merely whether both return a plan.
Include:

- Sharing that is profitable only when selected jointly.
- Reader-dependent summary sizes, evaluation rates, and retention.
- Multiple pane widths, alignment failures, and overlapping windows.
- Fused candidates, rollup dependencies, and cycles introduced by reuse.
- All supported materializations and internal-node assignments.
- Composed accuracy, shared scans, raw horizons, and global capacity limits.
- Zero-price resources, numeric tolerances, budget stops, and valid solver bounds.

For any relaxed formulation, check on enumerated plans that its assigned cost
never exceeds full cost and that every generated cut is valid. Measure planning
time, peak memory, model size, solver gap, and validation rounds on representative
workloads before choosing stronger decomposition or configuration restrictions.
