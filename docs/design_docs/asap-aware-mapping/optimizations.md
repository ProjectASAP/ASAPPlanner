# Dimensions of optimization

ASAP-aware mapping should support several largely orthogonal dimensions of optimization. Some of these are specific to sketch-based summaries, some are related to subpopulations/grouping keys.

- (sketch) Which sketch/summary to select (e.g. KLL vs DDSketch)
- (sketch) How to configure the summary
- (sketch, subpopulation) Use a subpopulation sketch (e.g. Hydra, OmniSketch) or maintain a sketch per subpopulation
- (sketch, subpopulation) Single-level Hydra (what the paper talks about) or multi-level Hydra (e.g. CMS on top of CMS on top of CMS)
- (sketch, time) Sliding window sketches (e.g. Promsketch) vs exact treatment of time
- (sketch, time) Sliding window vs tumbling window computation (this is specific to ASAPCollector and ASAPQuery's precompute engine)
- (subpopulation) Hierarchy of summaries (AHA) vs treating hierarchical subpopulations independently
- (sketch) Combining computation between part from the sketch, and part from the raw data to meet an accuracy target

Some of these are described below with examples.

## Using a subpopulation sketch

Queries often compute the same statistic over many subpopulations:

```text
latency by service
latency by region
latency by customer
```

There are at least two ways to use summaries here:

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

However, sharing is not always free. A shared result may require additional maintenance, storage, or coordination. Thus, the planner should consider both candidates. The cost model can compare them in the context of the whole workload.

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

## Sharing Across Group-By Levels

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
