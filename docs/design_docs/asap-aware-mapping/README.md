# ASAP-Aware Mapping

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

## Glossary

- **Pre-ASAP plan**: The logical query plan before ASAP-aware optimizations are considered.
- **Post-ASAP plan**: A logical plan that includes one or more ASAP-aware choices, such as summaries, sharing, roll-ups, or semantic rewrites.
- **Target Sub-DAG**: A sub-DAG of the pre-ASAP plan which is a candidate to be replaced by a post-ASAP sub-DAG
- **Replacement Sub-DAG**: A candidate post-ASAP sub-DAG to replace a target sub-DAG. For example, a quantile aggregation may have KLL, DDSketch, and exact aggregation as alternatives.
- **ReplacementStrategy**: A rule to recognize a target Sub-DAG and produces one or more valid replacement Sub-DAGs.
- **Candidate Plan**: A complete post-ASAP plan formed by choosing compatible ReplacementStrategies across the plan.
- **Cost Model**: A model used to compare valid candidate plans according to criteria such as storage, update cost, query latency, and accuracy.

The distinction between **ReplacementStrategy** and **Candidate Plan** is important. A ReplacementStrategy is a local choice at one decision point, while a candidate plan is a complete plan that combines choices across all relevant decision points.

---

# High-level Design

ASAP-aware mapping follows this flow:

```text
Pre-ASAP query plan
        |
        v
Discover target sub-DAGs
        |
        v
Generate valid replacement sub-DAGs
        |
        v
Check compatibility between replacement sub-DAGs
        |
        v
Build search space of candidate plans
        |
        v
Apply accuracy and semantic constraints
        |
        v
Estimate costs of candidate plans
        |
        v
Rank and select post-ASAP plans
```

---

## Detailed design

The design is split into focused documents:

- [Key concepts](key_concepts.md) defines target sub-DAGs, replacement sub-DAGs,
  replacement strategies, candidate plans, and the cost model.
- [Searching over plans](searching_over_plans.md) explains how the planner preserves,
  combines, checks, costs, and ranks alternatives across a workload.
- [Optimizations](optimizations.md) describes summary selection, parameterization,
  subpopulation and time organization, roll-ups, sharing, semantic rewrites, and hybrid execution.
- [Summary properties](summary_properties.md) lists the capabilities used to determine whether
  summaries and optimizations can be composed safely.
- [End-to-end accuracy guarantees](end-to-end-accuracy-guarantees.md) specifies the typed
  guarantee IR, sketch contracts, composition rules, target checking, and fail-closed boundaries.
- [Workload demand and summary lifecycle](workload-demand-and-summary-lifecycle.md) separates
  query demand from data workload and defines ephemeral, prepared, shared, and continuously
  maintained summary-state alternatives.
- [Explainability](explainability.md) describes how the planner reports available replacements
  using the same candidate space it optimizes.

---

# Goals, Design Principles, and Non-Goals

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

### 3. Explainability

The same information used to construct candidate plans should also answer questions such as:

- Can this aggregation use a sketch?
- Which summary families are valid?
- Can these two computations share work?
- Can one group-by result be rolled up into another?
- Can a semantic rewrite expose additional reuse?
- Where in the query plan are these opportunities available?

---

## Design Principles

The design should follow several principles.

1. Avoid premature optimization: Do not commit to one sketch, rewrite, or sharing decision before interactions with the rest of the plan are visible.
2. Separate legality from cost: Transformation logic determines what is valid. The cost model determines what is desirable.
3. Do not force approximation: Approximation is an option, not an assumption.
4. Model optimization dimensions independently: Summary family, summary parameters, grouping strategy, time organization, sharing, and semantic rewrites should be composable dimensions whenever possible.
5. Make planner explainable: The planner should be able to explain which optimizations are possible and where they apply based on the same candidate space used for optimization.
6. Make summary capabilities explicit: Properties such as mergeability, subtractability, deletion support, time awareness, and composability should drive transformation legality.

## Non-Goals

ASAP-aware mapping is not responsible for:

- assigning CPU cores,
- assigning memory budgets to execution nodes,
- choosing machine placement,
- scheduling execution,
- managing runtime admission control,
- or performing low-level execution tuning.

Those decisions belong to later physical planning or runtime layers.
