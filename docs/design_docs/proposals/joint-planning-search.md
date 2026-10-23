# ASAPPlanner: Memo-Based Joint Optimization with MILP

**Status:** Design proposal

**Scope:** Multi-query optimization across exact operators, ASAP primitives, window implementations, sharing, and materialization

**Audience:** Planner designers and implementers

## 1. Motivation and Problem Statement

ASAP primitives introduce optimization opportunities: a summary can replace expensive exact computation, one summary can answer several queries, and compatible queries can reuse panes, rollups, scans, or retained raw data. These candidates can reduce CPU, memory, storage, and evaluation latency.

They also enlarge the search space. Enumerating and pricing every complete workload plan produces combinatorial growth. Planning each query independently can miss sharing and account for a common physical producer multiple times.

**ASAPPlanner should represent candidates compactly and select a minimum-cost valid physical plan for the workload.** Optimality is relative to the generated candidate set, configured models, and solver tolerances.

### Goals

1. Avoid repeated exploration of identical or proven-equivalent logical sub-expressions.
2. Represent ASAP-aware reuse explicitly, including reuse between nonidentical logical expressions.
3. Optimize workloads under resource prices or a specified objective with hard resource constraints.

The coupled decisions include:

- Exact computation, exact accumulator state, or approximate summaries such as KLL.
- Legal window implementations and their configurations.
- Shared builds, pane chains, scans, rollups, and per-source raw retention.
- Materialization timing, storage medium, and lifetime.
- End-to-end accuracy, latency, and deployment constraints.
- Shared-producer costs counted once per actual execution or retained state, with reader-specific work counted separately.

### Non-goals

- Inventing arbitrary sketches or window algorithms.
- Assuming a closed-form linear cost/size/accuracy relationship.
- Substituting MILP estimates for physical-plan validation.
- Proving optimality over ungenerated configurations or all imaginable plans.
- Implementing runtime scheduling or retention services inside the optimizer.

## 2. Terminology

### 2.1 Logical expressions, memo groups, and candidates

Terminology follows [planner-layering.md](planner-layering.md):

| Term | Meaning |
| --- | --- |
| **Logical expression** | A node and its input sub-expressions in the language-independent `LogicalDAG` (Pre-ASAP IR). It specifies selectors, transformations, predicates, aggregations, grouping, window semantics, and output schema, while preserving source-language behavior such as series identity, evaluation timing, and missing-data semantics. It contains no summary or materialization choice. |
| **Memo group** | One distinct logical sub-expression and its output contract. Proven-equivalent logical expressions can have the same group identity. The current implementation hash-conses eligible identical expressions; it does not implement a general equivalence saturation engine. |
| **Implementation candidate** | One way to implement a group over separately chosen child groups. It owns private nodes, records dependencies and rule origin, and can carry coupling sites and accuracy obligations. A fused candidate can implement several groups. This corresponds to `ImplementationCandidate` in the memo code. |
| **Whole-workload candidate** | A DAG for the whole workload, as defined by the layering document: a `LogicalDAG`, logical `LogicalASAPDAG` (Post-ASAP IR), or timed `PhysicalASAPDAG`, depending on the stage. Its candidate sets are `CandidateLogicalDAGs`, `CandidateLogicalASAPDAGs`, and `CandidatePhysicalASAPDAGs`. |
| **Shared producer** | A physical computation or maintained intermediate state that compatible selected readers can reuse, including its upstream dependencies. Examples are a summary build, pane chain, scan, or finer result used by rollup. It need not itself be a query result. |
| **Coupling site** | A shared decision affecting several implementation candidates. Equal site keys identify one decision; thresholds enumerate legal configurations. Existing kinds include `SharedBuildSize`, `PaneChain`, `ScanLabels`, and `RawHorizon`. |
| **Physical plan** | One selected whole-workload `PhysicalASAPDAG`, produced by physical ASAP-aware optimization and selected at Stage 3. It has physical operator structure, execution timing, materialization assignments, and query roots. Shared physical nodes have multiple consumers. Execution remains downstream of ASAPPlanner. |

Below, “candidate” means a local implementation candidate when discussing groups, and a whole-workload candidate when discussing stage outputs. The memo compactly represents combinations of local candidates instead of constructing every whole-workload DAG.

A group's logical result remains fixed. Its exact candidates preserve that result exactly; approximate candidates implement it under explicit accuracy contracts. An approximate estimate is not an exactly equivalent logical expression. Different reader accuracy requirements can require different candidates without changing the requested statistic.

### 2.2 Three distinct kinds of reuse

| Mechanism | Condition | Representation | Example |
| --- | --- | --- | --- |
| Memo equivalence | Expressions have the same logical semantics. | One logical identity with implementation candidates. | Two occurrences of the same filtered sum over the same window. |
| Conventional CSE | Identical computation has compatible semantics and execution context. | One physical producer with multiple consumers, when the selected physical realization reuses it. | A common scan or identical exact aggregate. |
| ASAP-aware sharing | Different requested results can consume compatible intermediate state. | Separate result groups linked to a common producer/configuration. | P95 and P99 read one KLL; different window lengths reuse panes. |

Memo grouping deduplicates logical identities and associated search work. It does not automatically deduplicate arbitrary search states with different readers, materializations, or accuracy obligations, and it does not force runtime materialization. CSE and ASAP-aware sharing expose physical reuse candidates without equating different outputs.

## 3. ASAPPlanner Workflow

This is the target workflow. The current memo builder receives Pass 1 candidates; moving initial group construction before Pass 1 is a proposed change, not a description of today's call order.

```text
Stage 0: frontends -> language-independent LogicalDAGs
    |
Build initial memo
    - Intern eligible identical / proven-equivalent sub-expressions
    - Record roots, group dependencies, semantics, and reader requirements
    |
Stage 1 / Pass 1: generate per-computation candidates
    - Exact, exact accumulator state, summary families/sizes, fusion
    - Preserve logical output and approximation contracts
    |
Stage 1 / Pass 2: add ASAP-aware CSE candidates
    - Identical expressions, summary capability, window composition
    - Rollup, exact top-k reuse, scan widening, raw-horizon interactions
    - Record compatible producers/configurations and dependencies
    |
Stage 2: generate legal physical candidates
    - Physical operators, materialization timing, medium, and lifetime
    - Check window configurations against physical capabilities
    |
Analyze coupling across all generated candidates
    - Separate components only when objectives and feasibility factor
    |
Stage 3: joint MILP selection
    - Choose candidates, producer configurations, and reader attachments
    - Enforce accuracy, latency, dependencies, and resource constraints
    |
Reconstruct and validate the selected PhysicalASAPDAG
    - Refine/re-solve when required, or return a validated fallback/status
```

### 3.1 Stage 0: Normalize and build the initial memo

Frontends preserve source-language semantics. Canonicalize only where equivalence is established: names, predicates, operand order, or windows cannot be rewritten merely because they look similar. Floating-point behavior, nulls, missing samples, and evaluation context can prevent apparently commutative rewrites.

Intern distinct logical sub-expressions, verifying equality after hashing. Keys cover relevant source/input identity, arguments, filters, grouping, schema, series identity, window width/offset/alignment, and time semantics. Include late-data or ordering policies where they affect results. Current identity rules leave keyless nodes, such as range windows, separate per reader.

Store accuracy targets and evaluation requirements as reader/root obligations. Include a requirement in identity only if it changes the logical semantics; otherwise retain it in candidate-generation/cache context. Do not merge P95 with P99 merely because both can use KLL.

### 3.2 Stage 1, Pass 1: Generate local candidates

Generate per-computation candidates against group identities rather than composing complete workload plans. For eligible aggregates, include the exact implementation: exact accumulator state when supported and constructible, otherwise the original exact computation. Add supported summary families and sizes and fused-computation candidates.

Record each candidate's private nodes, logical output, child groups, guarantees, and accuracy obligations. Cache generation by group and every relevant context value, including reader requirements. This avoids repeated generation without dropping sizes needed by stricter readers. It also avoids the Cartesian product across unrelated groups.

### 3.3 Stage 1, Pass 2: Discover sharing opportunities

Use the ASAP-aware CSE rules from the layering design. Identical expressions are already group identity in the memo. Summary capability, window composition, rollup, exact top-k reuse, and scan widening add implementation candidates or dependencies between groups.

Match producer signatures using summary input data, family, reduction, grouping, item/weight, filters, and applicable window semantics. Requested quantile levels differ at estimation, but their build may match. Window compatibility is checked separately from summary capability.

For each opportunity, record:

- Producer identity, input/update semantics, and upstream dependencies.
- Finite configurations: summary size, pane width, retention, or scan labels.
- Compatible reader candidates and any required exposed state or exact input.
- Legal materializations to be resolved by Stage 2.
- Usage inputs and contracts needed for Stage 3 pricing, accuracy, and latency.

Pass 2 generates logical window-composition candidates; Stage 2 does not invent pane algorithms. Raw retention is a shared accounting dependency, not necessarily an executable operator. Sharing remains optional: independent physical instances remain selectable when supported and useful.

### 3.4 Stage 2: Physical candidates and materialization

Stage 2 makes physical operator and materialization decisions; it does not select the cheapest valid candidate. For example, exact top-k may lower to sort and limit. Materialization is assigned to individual eligible nodes/sub-DAGs, rather than one label for an entire query.

| Mode | Meaning | Cost drivers |
| --- | --- | --- |
| `NotMaterialized` | Run at query time without retaining an independently reusable output. | CPU and transient memory at the actual execution rate. |
| `MatAtQuery(Memory)` | Build once for applicable readers within an evaluation. | Per-evaluation build and memory held during the evaluation. |
| `MatAtIngestion(Memory)` | Maintain as input arrives and retain state for readers. | Updates, retained state, and reads. |
| `MatAtQueryKeptAcrossEvaluations(Memory)` | Build at query time and keep state across evaluations. | Build, subsequent updates/refresh, retention, and reads. |

Use the actual `Materialization` enum and `Medium`. Ingestion eligibility depends on `DataWorkload`, recurrence, predictability, input support, and window/phase legality. Current Stage 2 requires ingestion-time builds below ingestion-time builds; shared non-summary nodes below them use the permitted unmaterialized form. Distinguish execution at ingestion time from forcing every upstream output to be materialized.

The current retained-query-time mode requires deployment capability and an eligible tumbling-pane chain building one new pane per evaluation. It is not a generic cache mode for arbitrary nodes. Current physical generation also offers query-time sharing for eligible non-summary sub-DAGs. Disk placement, arbitrary cache policies, parallelism, partitioning, and resource management must not be claimed implemented merely because they appear in the broader design.

Following the layering document, earlier stages prune provably invalid candidates with reasons; Stage 3 selects among valid candidates. Cost-based dominance is a Stage 3 search optimization and needs a proof preserving every external sharing and resource obligation.

## 4. Window Semantics, Algorithms, and Materialization

Window semantics, window implementation, and materialization are separate dimensions, subject to compatibility constraints.

- **Semantics:** width, offset, evaluation timestamps, endpoint convention, and time alignment.
- **Implementation:** raw recomputation, a supported direct sliding-state algorithm, fixed-width panes, or a supported EH algorithm.
- **Materialization:** when internal state is built, maintained, retained, and discarded.

A 15m trailing window evaluated every 1m is sliding-window semantics. It may be implemented using 1m tumbling panes maintained at ingestion, with query-time merge and estimation. Width and step both 15m give tumbling semantics when aligned.

Workload vocabulary matters: `QueryRecurrence` specifies request cadence. `RangeEvaluation { step_ms, span_ms }` specifies evaluation steps inside one request. A range request can contain many evaluation timestamps. Neither request rate nor a legal pane grid alone gives the number of computations; pricing must account for both and any reuse.

### Panes and EH buckets

A **pane** is an aligned, non-overlapping fixed-width interval and its data or aggregate/summary state. A configuration defines width, origin, retention, and reconstruction rules. Partial boundary panes require explicit treatment: finer panes, raw boundary data, or an authorized approximation. Divisibility alone is insufficient when boundaries are misaligned.

An **EH bucket** summarizes a contiguous portion of a stream according to a specific exponential histogram algorithm. Size levels typically grow geometrically; standard EH sizes commonly count represented items rather than fixed time durations. Merging, expiration, partial-boundary treatment, and error guarantees are algorithm- and operator-specific. EH is not a generic replacement for panes or an arbitrary KLL.

Pane width, EH bucket size, and summary capacity are different parameters. Multiple pane widths may coexist, including a hierarchy of fine panes merged into coarser panes. The candidate generator defines which hierarchies exist in the search space; the solver cannot invent an absent composition.

For example, 15 × 1m, 5 × 3m, and 3 × 5m can represent different candidates for an aligned 15m interval. Complete 5m panes alone cannot generally reconstruct a window whose endpoint moves every 1m. A multi-width composition needs explicit coverage and no-double-counting checks.

| Window implementation | Ingestion work, when maintained there | Evaluation work |
| --- | --- | --- |
| Direct sliding state | Add records and expire contributions using a supported algorithm. | Read/finalize state. |
| Pane-based | Update active panes and maintain retention. | Select and merge panes, then finalize/estimate, unless those results are also maintained. |
| EH-based | Update, merge, and expire buckets. | Apply the supported window estimator. |

This table describes possible implementations, not a support matrix. Current code supports tumbling-pane/pane-chain generation and summary computation. General direct sliding-state algorithms and EH are not implemented by that support. Arbitrary multi-layer pane generation also remains an extension. The downstream deployment supplies runtime scheduling, input binding, and retention behavior.

Legality checks include mergeability, alignment/phase, coverage, ingestion support, historical evaluations, retention, and composed error. KLL pane merging does not provide reversible deletion of arbitrary old samples.

## 5. Cost and Accuracy Model

### 5.1 Cost attribution

Stage 3 measures `ResourceUsage`, then applies `PlanSelectionSettings.cost_model`. The objective must agree with those configured models, rather than introducing an independent MILP pricing model.

| Existing cost model | Objective |
| --- | --- |
| `UsageOverTime(ResourceUnitCost)` | Price resource usage per second, including occupancy over time. |
| `ProvisionedCapacity(ResourceUnitCost)` | Price provisioned peak memory, peak CPU cores, and disk. |
| `ClusterCapacity(ClusterLimits)` | Enforce memory/disk/optional-core limits and use `LowestLatency`, `LeastMemory`, or `ParetoFront`. |

A Pareto request requires a frontier computation; one scalar MILP solution does not return the whole frontier. Current memo MILP supports usage-over-time pricing; unsupported cost models route to legacy search. The other formulations below are extensions, not existing MILP capabilities.

Assign each charge to one physical owner:

1. Producer build/update, retained state, and maintenance.
2. Reader-specific merge, read, estimation, and finalization.
3. Nonshared physical instances.
4. Source-level retention and other plan-level charges.

Use consistent per-second rates, as in current code, or integrate all terms over one explicit billing horizon. Conceptually:

```text
C = p_cpu × CPU + p_mem × MemoryTime + p_disk × DiskTime
  + p_io × IO + other explicitly priced usage
```

Occupancy over time is different from peak capacity. Without prices, use a specified objective under quotas; do not add CPU-seconds and bytes without defined weights.

“Charge once” means once per actual execution or retained state. Query-time recomputation can occur repeatedly. A shared build's size, retention, and execution rate depend on its selected readers and ancestors' materializations. Constant coefficients are sufficient only when that context is fixed or explicitly represented by configuration/reach variables.

Model the union of actual reader triggers, including phases and reuse scope. Readers on aligned 60s and 90s schedules have union rate `1/60 + 1/90 - 1/180`; a 30s gcd grid is not their execution rate. Current pricing remains authoritative if its schedule approximation differs; changing that policy requires a separately validated model change.

Per-source raw retention uses the selected storage policy and longest required horizon, not duplicated per-query copies. Longer retained state and lifetime-dependent temporary memory can couple otherwise separate computations. Freeze measured calibration inputs before search.

### 5.2 Accuracy

Accuracy is an end-to-end query-root requirement evaluated by the configured accuracy model. Preserve error type, probability/failure parameters, evidence, and composition rules. A locally admissible sketch does not establish a valid ratio, arithmetic expression, or window approximation.

Filter known-incompatible configurations, then encode proven composition bounds or finite feasible-combination tables where practical. Otherwise use sound validation and refinement. Summary error and window error cannot be assumed to add without a valid bound. Exact-only obligations must remain enforceable.

### 5.3 Latency

Use the configured latency model. Current Stage 3 models per-evaluation latency as the sum of reached query-time node work, counted once per node for that query, plus the longest applicable ingestion-state compaction. It assumes elastic CPU without queueing; fixed recurrence also imposes a repeat-interval check. Range-request validation follows the evaluator's declared per-step/request semantics.

This is not a sum of all active workload costs and is not a general parallel-runtime critical-path model. If a future deployment uses parallel branches, queueing, or visibility delays, encode its precedence/scheduling bounds and validate them with the corresponding evaluator. Do not silently change latency semantics in the MILP.

## 6. MILP Formulation

This base formulation explains ownership, coverage, and reader attachments. Current code additionally models consumer reach, private materialization slots, per-reader copies, and context-dependent charges. These extensions are required whenever the base model's constant costs do not describe the physical plan.

### 6.1 Sets and parameters

| Symbol | Definition |
| --- | --- |
| `Q` | Workload queries. |
| `G` | Logical memo groups. |
| `C_g` | Legal physical candidates owned by group `g`, with local materialization assignments. |
| `B` | Potential shared producer instances or explicit shared accounting states. |
| `S_b` | Finite configurations of `b`, such as summary size, pane width, retention, and materialization. |
| `I(g,c)` | Groups implemented by candidate `(g,c)`, including fusion coverage. |
| `R(g,c)` | Required input groups, conditional on choosing `(g,c)`. |
| `J(g,c)` | Producer roles required by `(g,c)`. |
| `A(g,c,j)` | Compatible producer/configuration pairs for role `j`. |
| `d[g,c]` | Private incremental cost, excluding shared producers. |
| `f[b,s]` | Producer configuration's owned build/update/retention cost. |
| `r[g,c,j,b,s]` | Role-specific reader cost not already included elsewhere. |

Costs use one common horizon. Finite `S_b` permits lookup-table costs without a linear size/cost function. Configuration enumeration is itself a search-space cost. If `f` depends on reader sets, trigger rates, or ancestors' choices, represent those conditions explicitly or compute the charge with reach/auxiliary variables; it is not an unconditional constant.

### 6.2 Decision variables

```text
a[g]             ∈ {0,1}   group g is required
x[g,c]           ∈ {0,1}   candidate c is selected for group g
y[b,s]           ∈ {0,1}   producer b is enabled at configuration s
z[g,c,j,b,s]     ∈ {0,1}   candidate (g,c) uses (b,s) for role j
```

Materialization is part of `c` or `s`, or represented by explicit legal assignment variables such as `x[g,c,m]`. Do not maintain two representations that can disagree. A multi-node implementation can have several materialization slots rather than one mode for every private node.

### 6.3 Query coverage and group selection

Let `Roots(q)` be the group references needed for query `q`, including multiple references in a scalar query root:

```text
a[g] = 1                                      for g ∈ Roots(q), q ∈ Q
```

Without fusion, select one implementation for an active group and none for an inactive group:

```text
Σ_{c ∈ C_g} x[g,c] = a[g]
```

Current candidates can fuse several logical computations. The general form is:

```text
Σ_{(h,c): g ∈ I(h,c)} x[h,c] = a[g]
```

A fused candidate covers absorbed groups; do not additionally select standalone implementations for them. Every selected candidate requires its inputs:

```text
a[h] ≥ x[g,c]                                 for h ∈ R(g,c)
```

An active group must be justified by a query root, selected consuming candidate, upstream producer requirement, or fusion coverage. Add reverse support/flow constraints so unused groups cannot activate merely because prices are zero. Support must include all valid dependency kinds, not only original logical parents. Enforce acyclicity or construct a candidate dependency space with a proved ordering.

### 6.4 Sharing and configuration compatibility

For each required producer role:

```text
Σ_{(b,s) ∈ A(g,c,j)} z[g,c,j,b,s] = x[g,c]       for j ∈ J(g,c)
z[g,c,j,b,s] ≤ y[b,s]
```

Use a separate equality per role: a candidate might need both a scan and a pane chain. Candidates running locally have no shared role, or use explicit local-fallback variables. An empty compatibility set makes that required-role candidate invalid.

If producer instance `b` can have only one physical configuration:

```text
Σ_{s ∈ S_b} y[b,s] ≤ 1
```

Represent coexisting states as separate instances or explicitly permit multiple configurations. In particular, this is not a global one-pane-width constraint.

Prevent unused producers:

```text
y[b,s] ≤ Σ selected consumer attachments requiring (b,s)
```

The sum includes downstream producer attachments where producers consume other producers. Their upstream dependencies, exact-input requirements, and exposed-state requirements must also be encoded. Compatibility includes grouping, reduction, inputs, windows, family, capacity, freshness, and timing.

### 6.5 Objective: pay once for shared producers

For the base constant-cost case:

```text
min  Σ_{g,c} d[g,c] × x[g,c]
   + Σ_{b,s} f[b,s] × y[b,s]
   + Σ_{g,c,j,b,s} r[g,c,j,b,s] × z[g,c,j,b,s]
```

The producer's owned charge appears once, regardless of reader count. Exclude its cost from readers' private coefficients. Reader-specific work is charged per actual computation, not blindly per syntactic occurrence; another shared merge result needs its own ownership if reused.

For context-dependent rates and memory lifetimes, extend the model using consumer-reach variables, rate/configuration variables, and bounded linearizations. For example, binary `v = u AND w` requires:

```text
v ≤ u
v ≤ w
v ≥ u + w - 1
```

Raw-retention and pane-retention maxima need bounds linked to selected readers. Lower-bound-only maximum constraints are tight only when the objective or other constraints enforce minimization of that variable; otherwise encode its intended value explicitly.

### 6.6 Accuracy, validity, and resource limits

- **Accuracy:** compatibility filtering plus valid root-level composition constraints or a sound refinement loop.
- **Physical support:** selected operations must have supported physical implementations.
- **Materialization:** freshness, lifetime, update/refresh behavior, storage capability, and upstream timing must be consistent.
- **Windows:** validate alignment, pane coverage, offsets, historical reads, retention, and any algorithm-specific error contracts.
- **Reuse:** enforce exact-child/state-exposure conditions for rollup and top-k reuse.
- **Resources:** constrain CPU, memory, disk, and other requested budgets with the configured concurrency and resource model.
- **Latency:** enforce the configured per-query bounds and applicable recurrence checks.

For truly additive resource `k`, a capacity constraint can take the form:

```text
Σ u_x[k,g,c] × x[g,c] + Σ u_y[k,b,s] × y[b,s]
  + Σ u_z[k,g,c,j,b,s] × z[g,c,j,b,s] ≤ U[k]
```

Peak memory and concurrent CPU generally need more than a sum of average-use coefficients. Distinguish resident state from temporary lifetimes and overlapping tasks. Extend cost models only with validated usage accounting.

### 6.7 Why not optimize each group independently?

Two groups can each run a private KLL or read one common KLL build. A reader's incremental cost depends on whether another candidate already enables that build and whether it changes the required size, trigger rate, or retention.

A DP retaining only the cheapest local candidate per group can lose the joint optimum. DP with complete sharing context is possible, but its states can become large. MILP jointly selects discrete configurations; local caching or DP remains useful for parts that genuinely decompose.

## 7. Worked Example: P95 and P99 Quantiles

Assume identical filtered input, grouping, and aligned windows:

```text
Q1: quantile_over_time(0.95, stream[15m]), every 1m
Q2: quantile_over_time(0.99, stream[15m]), every 1m

G95: Quantile(q=0.95, input=X, window=15m)
G99: Quantile(q=0.99, input=X, window=15m)
```

The result groups differ. Pass 1 adds exact and supported KLL candidates. Pass 2 discovers compatible shared builds:

```text
                    KLL build(X, 15m, size=s)
                         /             \
                  estimate P95      estimate P99
                       |                 |
                      Q1                Q2
```

Illustrative costs per common horizon, assuming all listed choices meet requirements:

| Choice | Build cost | Reader costs | Total |
| --- | ---: | ---: | ---: |
| Separate KLL builds | 8 + 8 | 1 + 1 | 18 |
| One shared KLL build | 8 | 1 + 1 | 10 |
| Separate exact computations | 12 + 12 | included | 24 |

If a reader's target requires more capacity, choose a compatible larger shared build, supported private builds, or an exact candidate. Quantile level alone does not determine required KLL capacity; the accuracy model and target do.

### 7.1 Extending the example to panes

A supported candidate maintains 1m KLL panes at ingestion and merges the latest 15 at evaluation. Both readers can use the same panes and, if eligible, the same merge result. Their estimation remains distinct. Window composition is generated in Pass 2; Stage 2 determines its materializations and physical validity.

For different window lengths, the chain must cover the longest selected reader, and each reads the appropriate panes. Other pane widths or hierarchies require explicit generated candidates and valid boundary handling. Window compatibility is independent of sketch compatibility.

### 7.2 Example binary decisions

With one producer role named `summary` and one compatible configuration `s`:

```text
x[G95, KLL-reader] = 1
x[G99, KLL-reader] = 1
y[Shared-KLL(X,W), s] = 1
z[G95, KLL-reader, summary, Shared-KLL, s] = 1
z[G99, KLL-reader, summary, Shared-KLL, s] = 1
```

The objective charges the producer once and adds the two readers' incremental costs. Configuration `s` includes or consistently references materialization. A query-time rebuilt full-window KLL is distinct from an ingestion-maintained pane chain; do not assume a full 15m sketch can directly delete expired samples every minute.

## 8. Decomposition and Search-Space Control

Build a coupling graph across all candidates, including child dependencies, fusion, shared producer choices, state exposure, common raw sources, and plan-level charges. Solve independent components only if objectives and feasibility factor. A shared capacity constraint or workload-wide calibration can prevent independent scalar optimization; retain the joint model or combine sufficient resource frontiers.

This component decomposition differs from master/subproblem decomposition such as LBBD.

1. Intern logical identities and compatible producer signatures with verified equality.
2. Remove provably invalid algorithm/window/materialization combinations with reasons.
3. Apply Stage 3 dominance only with the same external sharing interface and no worse relevant obligations; standalone price is insufficient.
4. Enumerate supported pane widths, capacities, retention configurations, and assignments without building complete-plan products.
5. Use a validated independent-query baseline as an incumbent where available; keep its true resource and accuracy checks.
6. Set solver time/gap limits and report incomplete search honestly.
7. Track candidate-generation, coefficient-generation, solver, and validation costs separately.

Representative parameter subsets or adaptive enrichment can reduce model size, but narrow the finite candidate space. A solver certificate applies only to the current inventory. Expanding that inventory requires solving the expanded problem; validation does not prove omitted configurations are irrelevant.

Start with a direct MILP where costs and checks can be encoded compactly. The current implementation uses a lower-bound model with lazy cost/validity refinement for difficult terms. That is an implementation strategy, not a mathematical requirement of MILP. LBBD is one possible formally justified master/subproblem design; an arbitrary validate-and-add-cuts loop is not automatically LBBD.

## 9. Plan Reconstruction and Verification

1. Traverse query roots, selected candidates, role attachments, and producer dependencies.
2. Construct one physical DAG, respecting timing-specific copies and selected materializations. Deduplicate only compatible physical instances and reject cycles.
3. Check root coverage, dependency support, execution capabilities, and state availability.
4. Run the configured accuracy evaluator on each complete query result.
5. Run the configured latency evaluator under its timing and resource assumptions.
6. Run the full cost/resource evaluator, including retention, temporary memory, and shared charges.
7. If validation fails or cost was underestimated, refine and re-solve under a bounded budget, or return a validated baseline with an explicit reason.

An invalid incumbent does not prove the entire workload infeasible. No plan found before timeout is also not an infeasibility proof. If a validated plan exists but the remaining model is exhausted, return that plan rather than reporting infeasibility.

For a relaxed model, keep a certified solver lower bound `L` and best validated cost `U`. Use `(U-L)/U` for positive `U`, with explicit zero-cost handling and numerical tolerances. A feasible solver objective is not automatically a certified lower bound when the solve stops early.

Feasibility cuts must exclude only proved-invalid combinations. Cost cuts must be conditioned on every decision affecting their charge, including materializations, reader attachments, and configurations. A cut based only on group candidates can be invalid if those candidates permit several physical realizations. Retain any feasible configuration excluded after evaluation and include its region in the optimality argument. Refine until the valid global bound closes the target gap or the budget expires.

A direct exact MILP and a validated matching evaluator can certify the finite-space optimum. A relaxed MILP plus validation of one plan cannot do so without sound refinement/bounds. Report modeled objective, validated cost, solver gap, global validated bound/gap where available, stop reason, and fallback routing distinctly.

## 10. Data Structures and Interfaces

Reuse the current architecture rather than adding a parallel optimizer framework. The following names map the conceptual model to the code inspected on `origin/main` at `900800fd`:

| Design concept | Current representation / boundary |
| --- | --- |
| Logical identity | `MemoGroup { node, children, parents, queries, roots_of }` and `GroupId`. |
| Group candidate inventory | `Memo { groups, candidates }`. |
| Implementation candidate | `ImplementationCandidate { implements, node, children, origin, coupling_sites, budget_share, requires_exact_children, rollup_of, materializations, requires_sites }`. |
| Shared configuration | `CouplingSite { key, kind }`, `CouplingKind`, and rule-specific thresholds/reader checks. |
| Physical choice | MILP `Choice`, including candidate identity, materialization assignment, coefficients, and copy/scan context. |
| Per-node usage and costs | Stage 3 pricing, `ResourceUsage`, `PlanningModels`, `PlanSelectionSettings`, and derived choice coefficients. |
| Selected output | `PhysicalCandidate` / `PhysicalASAPDAG`, `Selection`, and search/MILP reports. |

`SharedProducer`, `ProducerConfiguration`, and role attachments are formulation concepts. Existing sites and shared-node keys provide much of this representation. Introduce explicit records only where needed to express multiple suppliers, upstream producers, or ownership that existing structures cannot represent; do not duplicate identity or charge ownership in two catalogs.

A conceptual extension, when required, is:

```text
ProducerConfiguration {
    producer_identity, configuration_id
    family_or_operator, input_and_update_signature
    window_parameters, summary_parameters
    materialization_assignment, retention_and_freshness_contract
    required_inputs, compatible_reader_roles
    resource_profile, accuracy_contract
}

SelectedChoices {
    group_candidates, producer_configurations, reader_attachments
    reconstructed_PhysicalASAPDAG
    modeled_objective, certified_bound, validated_cost
    solver_status, validation_status, search_report
}
```

These are design sketches, not new public APIs or substitutes for `Selection`.

### Current module boundaries

Current main has moved the earlier `pass/subexpr` implementation into:

| Responsibility | Module |
| --- | --- |
| Group construction and memo data | `crates/plan-selection/src/search/memo/{mod,groups}.rs` |
| Pass 2 memo rules | `crates/plan-selection/src/search/memo/rules/{capability,windows,scans}.rs` |
| Reference memo search and construction | `crates/plan-selection/src/search/memo/brute_force.rs` |
| MILP model, coefficients, cuts, decoding, reporting | `crates/plan-selection/src/search/milp/{model,coefficients,cuts,decode,report}.rs` |
| Independent memo clusters | `crates/plan-selection/src/search/milp/clusters.rs` |
| Stage orchestration and legacy routing | `crates/plan-selection/src/search/legacy/stage1_to_stage3.rs` |
| Physical generation | `crates/physical-asap-optimization/src/lib.rs` |
| Local logical candidates | `crates/logical-asap-optimization/src/per_computation_candidates.rs` |

The PR's older base may not contain the refactored paths. See the inspected [memo code](https://github.com/ProjectASAP/ASAPPlanner/tree/900800fd8a7e57c35c84d365c209bd687407f146/crates/plan-selection/src/search/memo), [MILP code](https://github.com/ProjectASAP/ASAPPlanner/tree/900800fd8a7e57c35c84d365c209bd687407f146/crates/plan-selection/src/search/milp), and [current layering design](https://github.com/ProjectASAP/ASAPPlanner/blob/900800fd8a7e57c35c84d365c209bd687407f146/docs/design_docs/proposals/planner-layering.md).

Current `brute_force::memo` receives `PerComputationCandidates`, builds groups, and adds Pass 2 candidates. Search modes are `Legacy` (default), `MemoBruteForce`, and `MemoMilp`. The memo MILP uses `good_lp` with HiGHS, clusters under supported conditions, decodes choices, and evaluates complete plans through Stage 3 with lazy refinement. Initial construction before Pass 1, arbitrary pane hierarchies, EH/direct sliding algorithms, and broader MILP cost-model support remain proposed extensions.

## 11. Implementation Plan

These phases distinguish existing foundations from proposed changes.

| Phase | Deliverable | Exit criterion |
| --- | --- | --- |
| 1. Early memo foundation | Separate logical identity construction from Pass 1 attachment; move initial construction after Stage 0. | Eligible identical expressions intern once without changing semantics or losing reader obligations. |
| 2. Direct group generation | Adapt existing per-computation generation to emit group candidates, including fusion. | Explicit dependencies and contracts; no complete-workload Cartesian product. |
| 3. Sharing representation | Reuse sites/thresholds and shared-node identity; add role records only where necessary. | P95/P99 share a compatible build without merging result groups or duplicating charges. |
| 4. Physical choices and costs | Reuse legal materializations and Stage 3 accounting; extend supported configurations explicitly. | Same pricing and eligibility as authoritative physical generation/evaluation. |
| 5. MILP coverage | Maintain current selection/refinement path; extend role/dependency/resource coverage where needed. | Matches exhaustive and memo brute force on the same finite inventory. |
| 6. Scaling | Proven independent clusters, safe Stage 3 dominance, solver budgets, optional configuration enrichment. | Reports generation/model/solve costs without storing every workload candidate. |
| 7. Validation and window extensions | Reconstruct/validate every selected plan; implement new algorithms only with runtime support and guarantees. | Plans meet declared constraints or return a validated fallback/explicit failure status. |

## 12. Testing and Evaluation

### Correctness

- Eligible identical expressions share a group; P95 and P99 remain separate.
- Different accuracy obligations preserve the candidates needed by each reader.
- Compatible KLL readers share a build; incompatible inputs, grouping, filters, or windows do not share without an explicit valid conversion.
- Shared builds and shared merges are counted once at actual execution rates; reader-specific work is counted appropriately.
- Selected dependencies are active, unused groups/producers cannot activate, and fusion does not double-implement absorbed groups.
- Multiple producer roles, upstream producer dependencies, and cycles are handled correctly.
- Pane widths, phases, partial boundaries, offsets, and historical reads satisfy coverage; future EH candidates are tested only after supported contracts exist.
- Unsupported materializations are excluded; retained state obeys timing, freshness, and lifetime requirements.
- Tiny-instance MILP, memo brute force, and exhaustive search agree on minimum validated cost for the same inventory/models, with an explicit tie policy.
- Zero prices, capacities, numeric tolerances, timeout/no-incumbent cases, and fallback routes have correct status.
- For relaxed models, every tested plan's lower-bound price is no greater than full price, and generated cuts preserve all feasible cheaper configurations.

### Performance

Measure workload size against groups, candidates, producer/configuration identities, reader edges, variables, constraints, generation/solve/validation time, and peak planner memory. Compare independent-query planning with joint planning on modeled and measured resource savings. Vary query overlap, sketch sizes, pane widths, evaluation schedules, and materializations.

Report time-limited incumbent quality and certified gaps, modeled versus validated cost, and planning overhead versus execution savings. Evaluate billing and resource-constrained deployments only through supported engines; do not label a legacy fallback as MILP coverage.

## 13. Open Design Questions

1. Which accuracy compositions admit compact constraints, and which need sound evaluator-driven refinement?
2. Which operators support pane merge, direct expiration, or EH with the required guarantees and boundary behavior?
3. What is the shared-instance identity across evaluation rounds, and which freshness/update policies are executable?
4. Should resource and latency models retain current assumptions or add validated concurrency/critical-path models?
5. How should finite capacities, pane widths, retention horizons, and adaptive inventory expansion be chosen?
6. Which incompatible windows, update rates, or groupings can be connected through an explicit conversion candidate?
7. How should billing objectives, cluster objectives, accuracy margins, and planning budgets be prioritized?
8. When do model size and refinement measurements justify a formal LBBD or another decomposition, rather than the direct model?

## 14. Design Outcome

The memo represents logical identities and per-group implementation candidates. ASAP-aware CSE exposes shared computations through producer identities, coupling sites, and reader dependencies without conflating different results. MILP selects a consistent physical DAG and attributes each charge to the work or state that incurs it. The selected plan remains subject to the existing accuracy, physical, latency, and cost evaluators; optimality claims require a complete finite-space model or a sound refinement proof.
