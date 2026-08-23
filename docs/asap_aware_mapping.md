# ASAP-Aware Mapping

## Glossary

- **Pre-ASAP plan**: The logical query plan before ASAP-aware optimizations are considered.
- **Post-ASAP plan**: A logical plan that includes one or more ASAP-aware choices, such as summaries, sharing, roll-ups, or semantic rewrites.
- **Target Sub-DAG**: A region of the pre-ASAP plan where a transformation may apply.
- **Alternative**: One valid local realization of a Target Sub-DAG. For example, a quantile aggregation may have KLL, DDSketch, and exact aggregation as alternatives.
- **Transformation**: A rule that recognizes a Target Sub-DAG and produces one or more valid alternatives.
- **Candidate Plan**: A complete post-ASAP plan formed by choosing compatible alternatives across the plan.
- **Cost Model**: A model used to compare valid candidate plans according to criteria such as storage, update cost, query latency, and accuracy.

The distinction between **Alternative** and **Candidate Plan** is important: an alternative is a local choice at one decision point, while a candidate plan is a complete plan that combines choices across all relevant decision points.

---

## Overview

ASAP-aware mapping decides **whether and how a query intent can be answered using summaries instead of scanning raw data**.

Given a logical query plan, the mapping layer explores alternative plans that may use sketches, exact summaries, shared computation, roll-ups, semantic rewrites, or combinations of these techniques.

The input is a **pre-ASAP plan** describing what the query wants to compute. The output is a set of **candidate post-ASAP plans** describing different valid ways to realize that computation.

For example, a percentile query might be answered by:

```text
Quantile(latency, 0.99)

        ↓

KLL
DDSketch
exact aggregation
```

Which alternative is preferable depends on requirements such as accuracy, latency, storage, update cost, and the surrounding query workload.

ASAP-aware mapping therefore answers two questions:

1. **What transformations are valid?**
2. **What combinations of alternatives form useful candidate plans?**

It does **not** assign physical resources such as CPU or memory to operators. Physical resource allocation belongs to a later planning stage.

---

## Goals

The mapping layer should support three capabilities.

### 1. Discover valid alternatives

For each relevant part of a query plan, determine which exact or approximate realizations preserve the query's required semantics.

For example:

```text
DistinctCount(user_id)
    → exact distinct counting
    → HyperLogLog

Quantile(latency, 0.99)
    → exact quantile
    → KLL
    → DDSketch
```

A query intent may therefore have more than one valid alternative.

### 2. Explore interactions between alternatives

Optimization choices cannot always be made independently.

For example, the best summary for one aggregation may depend on:

- whether the result can be shared with another query,
- whether multiple group-by levels can be computed through roll-up,
- whether subpopulations share one summary or use separate summaries,
- whether a semantic rewrite exposes additional sharing,
- whether time windows can reuse common state,
- and whether another transformation changes the cost of maintaining the summary.

ASAP-aware mapping should therefore reason about **candidate plans as a whole**, rather than greedily choosing the best alternative at each node.

### 3. Explain applicability

The same information used to construct candidate plans should also answer questions such as:

- Can this aggregation use a sketch?
- Which summary families are valid?
- Can these two computations share work?
- Can one group-by result be rolled up into another?
- Can a semantic rewrite expose additional reuse?
- Where in the query plan are these opportunities available?

Applicability reporting should describe opportunities already discovered by the planner rather than duplicate the planner's reasoning in a separate rule system.

---

# Core Abstractions

## Target Sub-DAG

A **Target Sub-DAG** is a region of the pre-ASAP plan where a transformation may apply.

Examples include:

- an aggregation that could be answered by a sketch,
- two computations that could share common work,
- several group-by computations that could be related through roll-up,
- or an expression that has a semantically equivalent representation.

The target may consist of a single operator or several connected operators.

---

## Alternative

An **Alternative** is one valid local realization of a Target Sub-DAG.

For example:

```text
Target:

Quantile(latency, 0.99)

Alternatives:

KLL(latency)
DDSketch(latency)
ExactQuantile(latency)
```

Another example involves computation sharing:

```text
Target:

Aggregate(source, by=[service, region])
Aggregate(source, by=[service])

Alternatives:

1. Compute both independently

2. Compute:
      Aggregate(source, by=[service, region])
   then roll up to:
      Aggregate(..., by=[service])
```

An alternative represents a semantic choice, not necessarily an approximation.

---

## Transformation

A **Transformation** describes a class of plan changes.

Conceptually, a transformation answers:

> When a particular structure appears in a plan, what valid alternatives can replace it?

Examples include:

- replacing an aggregation with compatible summaries,
- sharing equivalent computation,
- rolling up between related group-by levels,
- rewriting an expression into an equivalent form,
- choosing different ways to organize subpopulations,
- choosing different time representations.

A transformation proposes valid alternatives. It does not decide which candidate plan is globally best.

---

## Candidate Plan

A **Candidate Plan** is a complete post-ASAP plan formed by choosing a compatible set of alternatives across the plan.

For example, one candidate plan may choose:

```text
Quantile implementation:
    KLL

Subpopulation organization:
    shared multi-subpopulation summary

Related group-bys:
    roll up from finer-grained aggregation

Semantic form:
    rewrite avg as sum/count
```

Another candidate plan may make different choices at any of these decision points.

The planner compares candidate plans, not isolated alternatives, when interactions between decisions matter.

---

## Cost Model

The **Cost Model** estimates the trade-offs of candidate plans.

Depending on the planning stage, costs may include:

- summary storage,
- ingestion or update work,
- query latency,
- raw-data processing,
- maintenance overhead,
- expected accuracy,
- and workload-dependent reuse.

The cost model is intentionally separate from transformation legality.

A transformation answers:

> Is this alternative valid?

The cost model answers:

> How attractive is the resulting candidate plan relative to other valid candidate plans?

This separation allows the same planning framework to support different cost models, including heuristic, analytical, and empirically learned models.

---

# Candidate Plan Search

The central design principle is that ASAP-aware mapping should **preserve alternatives long enough to reason about their interactions**.

Suppose a plan contains several independent-looking decision points:

```text
                 Query workload
                       |
          +------------+------------+
          |                         |
      Quantile                  Group-by
          |                         |
    KLL / DDSketch          independent / roll-up
```

Choosing KLL immediately because it appears locally cheapest may be wrong if another summary enables more efficient sharing elsewhere in the plan.

Similarly, deciding independently whether to share two computations may miss a better plan produced after semantic rewriting.

The planner should therefore construct a search space of local alternatives and evaluate the complete candidate plans formed from their compatible combinations.

---

## Shared Representation of Alternatives

Candidate plans often differ in only a small part of the overall query DAG.

For example:

```text
                  shared source
                       |
                 shared filters
                       |
                +------+------+
                |             |
              KLL         DDSketch
                |             |
                +------+------+
                       |
                shared remainder
```

The planner should represent common structure once and attach alternatives only at the decision points where plans differ.

Conceptually, each decision point forms an **alternative group** containing its local choices:

```text
Group A: Quantile implementation
    - KLL
    - DDSketch
    - exact quantile

Group B: Subpopulation organization
    - one summary per subpopulation
    - shared multi-subpopulation summary

Group C: Related aggregations
    - compute independently
    - compute once and roll up
```

A candidate plan is formed by selecting one compatible alternative from each relevant group.

This avoids representing every full plan independently when most of their structure is identical.

---

## Global Rather Than Local Decisions

The planner should evaluate alternatives at the plan level because several dimensions interact:

```text
summary family
    ×
summary parameters
    ×
subpopulation organization
    ×
roll-up structure
    ×
computation sharing
    ×
semantic rewrites
    ×
time representation
```

A locally optimal choice may prevent a globally better combination.

The planner should therefore retain local alternatives until enough context exists to compare the complete candidate plans they produce.

The design may prune clearly dominated alternatives, but pruning should preserve choices that could become useful because of interactions elsewhere in the plan.

---

# Mapping Query Intents to Summaries

A core responsibility of ASAP-aware mapping is translating logical aggregation intents into compatible summary families.

Representative mappings include:

```text
Count
    → exact counter

DistinctCount(field)
    → exact distinct accumulator
    → HyperLogLog-family summary

Quantile(field, q)
    → exact quantile accumulator
    → KLL-family summary
    → DDSketch-family summary

TopK(key, Count, k)
    → exact frequency aggregation
    → heavy-hitter summary such as SpaceSaving
```

These mappings are intentionally **one-to-many**.

For example:

```text
Quantile(latency, 0.99)

        ↓

Candidate realizations

    KLL
    DDSketch
    exact quantile

        ↓

Accuracy, storage, update cost,
query latency, and workload context

        ↓

Candidate-plan ranking
```

An exact implementation should remain a valid candidate when appropriate. ASAP-aware mapping is not simply a "replace operators with sketches" pass; it explores both exact and approximate realizations.

---

# Accuracy-Aware Parameterization

Choosing a summary family is only part of the decision.

A summary may itself have several valid configurations:

```text
Quantile(latency, 0.99)

        ↓

KLL
    ├── smaller state
    ├── medium state
    └── larger state

DDSketch
    ├── tighter relative error
    └── looser relative error
```

The configuration determines trade-offs among:

- accuracy,
- storage,
- update cost,
- merge cost,
- and query latency.

Given an accuracy target, the mapping layer should expose configurations that satisfy the target and allow the cost model to compare them with other valid plans.

Accuracy requirements therefore act as **constraints on the search space**, rather than as a separate decision made after a summary family has already been chosen.

---

# Design Dimensions

ASAP-aware mapping should support several largely orthogonal dimensions of optimization.

## Summary Family

Different summary algorithms may implement the same logical intent.

For example:

```text
Quantile
    → KLL
    → DDSketch
    → exact quantile
```

The planner should preserve these as alternatives rather than committing to one family before considering the rest of the plan.

---

## Summary Parameters

The same summary algorithm may admit multiple parameterizations.

Increasing summary size may improve accuracy while increasing memory and update cost.

Different parameterizations should therefore be represented as distinct alternatives when they lead to meaningful accuracy or cost trade-offs.

---

## Subpopulation Organization

Queries often compute the same statistic over many subpopulations:

```text
latency by service
latency by region
latency by customer
```

There are at least two broad ways to organize summaries.

### Per-subpopulation summaries

Maintain a separate summary for each subpopulation.

```text
service A → sketch
service B → sketch
service C → sketch
```

This is conceptually simple and provides strong isolation between groups.

### Shared multi-subpopulation summaries

Use a structure that represents many subpopulations together.

```text
                shared summary
              /       |       \
         service A service B service C
```

Hydra-style structures are one example of this approach.

These alternatives affect memory, update cost, query cost, accuracy, and compatibility with other transformations.

Subpopulation organization should therefore be modeled as a separate design dimension rather than being implicitly tied to a particular summary family.

---

## Hierarchical Summary Structures

Shared summaries may themselves have multiple levels.

For example:

```text
                 coarse summary
                 /            \
         intermediate       intermediate
             /                   \
        leaf summary          leaf summary
```

A single-level structure may be sufficient for some workloads, while hierarchical structures may scale better for others.

The hierarchy should be treated as a property of the plan rather than assumed to be fixed by the summary family.

---

## Time Organization

Windowed analytics introduces another independent design choice.

A query such as:

```text
p99 latency over the last 30 minutes
```

might be maintained using:

- tumbling windows,
- sliding-window computation,
- mergeable summaries over smaller time buckets,
- sliding-window-specific summaries,
- or exact treatment of time.

Time organization affects both accuracy and the ability to reuse summaries across queries with different windows.

It should therefore be modeled explicitly for windowed workloads.

---

## Group-By Roll-Up

Related aggregation levels may reuse computation.

Consider:

```text
latency BY service, region

latency BY service
```

If the underlying summary is mergeable, the finer-grained aggregation may be used to derive the coarser one:

```text
Raw data
   |
Aggregate BY service, region
   |
Roll up regions
   |
Aggregate BY service
```

instead of:

```text
Raw data ──→ Aggregate BY service, region

Raw data ──→ Aggregate BY service
```

Whether this transformation is valid depends on the properties of the underlying summary and how subpopulations are represented.

The planner therefore cannot treat roll-up as an isolated optimization. It must consider its compatibility with the selected summary and grouping organization.

---

## Hybrid Raw-Data and Summary Execution

Some accuracy requirements may be satisfied most efficiently using both summarized and raw data.

Conceptually:

```text
                Query
                  |
          +-------+-------+
          |               |
       Summary         Raw data
          |               |
          +-------+-------+
                  |
             final result
```

Examples may include refining a summary-based estimate using a limited raw-data scan, or keeping exact information for only part of the domain.

Hybrid execution introduces another point in the design space between fully summarized and fully exact computation.

---

# Cross-Query Optimization

Many important opportunities are visible only when considering multiple queries or multiple branches of a workload together.

## Common Computation

Equivalent computations should be shareable when doing so reduces total work.

For example:

```text
Query A: Filter(source, service="api") → Aggregate(...)
Query B: Filter(source, service="api") → Aggregate(...)
```

The shared portion can potentially be computed once and reused.

However, sharing is not always free. A shared result may require additional maintenance, storage, or coordination.

Both alternatives should therefore remain visible to the planner:

```text
compute independently
vs.
compute once and share
```

The cost model can then compare them in the context of the whole workload.

---

## Semantic-Equivalent Rewriting

Two queries that are logically related may not initially expose reusable structure.

For example:

```text
avg(x)
```

can be represented as:

```text
sum(x) / count(x)
```

A semantic rewrite may make it possible to share `sum` or `count` with other queries.

The planner should therefore treat the original and rewritten forms as alternative plans rather than assuming semantic rewrites are always beneficial.

For example:

```text
Original:
    avg(x)

Alternative:
    sum(x) + count(x)
        ↓
    final division
```

The rewritten form may incur more local work while enabling substantially more cross-query sharing elsewhere.

Only plan-level comparison can capture this trade-off correctly.

---

## Sharing Across Group-By Levels

Structural equality is not the only source of reuse.

Consider:

```text
Query A: Aggregate BY service, region
Query B: Aggregate BY service
```

The two operators are not identical, but their grouping keys are related.

If the aggregation is mergeable, Query B may be derived by rolling up Query A.

This creates reuse opportunities across **hierarchically related groupings**, not just identical subtrees.

The legality and cost of this transformation depend on:

- summary mergeability,
- grouping organization,
- summary accuracy behavior,
- and the relative maintenance and query costs of the two alternatives.

---

# Applicability Reporting

ASAP-aware mapping should make its discovered opportunities visible to users and other components.

For each relevant region of the plan, the planner should be able to report:

- which transformations are applicable,
- what alternatives each transformation introduces,
- what conditions make those alternatives legal,
- and which parts of the plan they affect.

For example:

```text
Aggregation: Quantile(latency, 0.99)

Applicable alternatives:
    - exact quantile
    - KLL
    - DDSketch

Additional opportunities:
    - share source filtering with Query B
    - reuse finer-grained aggregation through roll-up
```

Applicability is therefore a **view of the candidate search space**, not a second optimization engine.

This keeps explanations consistent with the plans the optimizer can actually produce.

---

# Summary Properties

To determine whether transformations are valid, ASAP-aware mapping needs a common description of summary capabilities.

Important properties include the following.

## Mergeability

Can two independently maintained summaries be combined to produce a summary of their union?

Mergeability enables:

- distributed aggregation,
- time-bucket merging,
- group-by roll-up,
- hierarchical summaries,
- and some forms of shared computation.

---

## Subpopulation Awareness

Can one summary represent multiple subpopulations and answer queries for individual subsets?

This determines whether shared multi-subpopulation designs are possible.

---

## Subtractability

Can one summary be removed from another?

Conceptually:

```text
Summary(A ∪ B) - Summary(B) → Summary(A)
```

Subtractability is useful for sliding windows, dynamic partitions, and incremental maintenance.

---

## Deletion Support

Can individual items be removed from a summary?

Deletion support may be necessary for:

- sliding windows,
- corrections,
- mutable datasets,
- and retractions.

This is distinct from subtracting one complete summary from another.

---

## Time Awareness

Does the summary natively model time or window semantics?

A time-aware summary may support operations that are difficult or expensive using a time-agnostic sketch.

---

## Linearity

Can the summary participate in linear combinations or related algebraic operations?

Linearity may enable composition, subtraction, hierarchical aggregation, or recovery of related statistics.

---

## Accuracy Under Continued Insertion

Does the summary's error behavior change as more items are inserted?

Some summaries provide guarantees largely independent of stream length, while others may degrade or require resizing.

The planner needs this information when selecting long-lived summaries.

---

## Composability

Can a summary serve as the input to another logical or summary operator?

For example:

```text
raw data
   ↓
summary A
   ↓
summary B
```

or:

```text
fine-grained summaries
        ↓
      roll-up
        ↓
coarser summary
```

Composability determines which multi-stage candidate plans are legal.

---

# Validity and Compatibility

Individual transformations may be valid in isolation but invalid in combination.

For example:

```text
summary family
    ✓ supports merge

grouping strategy
    ✓ supports multiple subpopulations

roll-up
    ?
```

The planner must verify that the complete combination preserves the intended semantics.

Compatibility checks may involve:

- whether a summary can be merged,
- whether its accuracy guarantees survive composition,
- whether the chosen grouping strategy supports roll-up,
- whether subtraction or deletion is required,
- whether a semantic rewrite preserves approximation guarantees,
- and whether time organization is compatible with the selected summary.

This motivates modeling summary capabilities explicitly instead of encoding assumptions inside individual optimization rules.

---

# Planning Flow

At a high level, ASAP-aware mapping follows this flow:

```text
Pre-ASAP query plan
        |
        v
Discover transformable regions
        |
        v
Generate valid alternatives
        |
        v
Check compatibility between alternatives
        |
        v
Build shared candidate-plan search space
        |
        v
Apply accuracy and semantic constraints
        |
        v
Estimate candidate plan costs
        |
        v
Rank or select candidate post-ASAP plans
```

Each stage has a distinct responsibility.

### Discovery

Find places where an alternative realization may exist.

### Alternative generation

Enumerate semantically valid replacements.

### Compatibility

Determine which alternatives can coexist.

### Constraint checking

Remove candidates that cannot satisfy accuracy or semantic requirements.

### Costing

Estimate the trade-offs of remaining candidates.

### Ranking

Expose the most attractive post-ASAP plans to the next planning stage.

---

# Non-Goals

ASAP-aware mapping is not responsible for:

- assigning CPU cores,
- assigning memory budgets to execution nodes,
- choosing machine placement,
- scheduling execution,
- managing runtime admission control,
- or performing low-level execution tuning.

Those decisions belong to later physical planning or runtime layers.

ASAP-aware mapping focuses on the logical question:

> **What summarized or shared computation could answer this workload, and which combinations are worth considering?**

---

# Design Principles

The design should follow several principles.

### Preserve alternatives

Do not commit to one sketch, rewrite, or sharing decision before interactions with the rest of the plan are visible.

### Separate legality from cost

Transformation logic determines what is valid. The cost model determines what is desirable.

### Treat exact computation as a candidate

Approximation is an option, not an assumption.

### Model optimization dimensions independently

Summary family, summary parameters, grouping strategy, time organization, sharing, and semantic rewrites should be composable dimensions whenever possible.

### Share the search space

Candidate plans should reuse common structure rather than duplicating entire DAGs for every combination of choices.

### Make applicability explainable

The planner should be able to explain which optimizations are possible and where they apply based on the same candidate space used for optimization.

### Make summary capabilities explicit

Properties such as mergeability, subtractability, deletion support, time awareness, and composability should drive transformation legality.

---

# Summary

ASAP-aware mapping is the logical optimization layer that bridges query intent and summary-based execution.

It transforms:

```text
"What does the workload want to compute?"
```

into:

```text
"What exact, approximate, shared, or rewritten plans could compute it?"
```

The key challenge is not simply matching an aggregation to a sketch. It is exploring a multidimensional design space in which summary choice, accuracy, grouping structure, reuse, semantic rewriting, time organization, and cross-query optimization interact.

The output is therefore best viewed not as one immediate rewrite, but as a **shared search space of valid candidate post-ASAP plans** that can be compared using a cost model and passed to later planning stages.
