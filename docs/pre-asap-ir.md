The pre-ASAP IR is deliberately narrower than SQL or any other source-language AST.
Only concepts that are semantically relevant to answering the query and selecting a summary
need to become first-class nodes.

Design principles:

1. Expose the query semantics dimensions that affect summary applicability, correctness, and cost.
2. If an operation changes presentation but does not change the semantic summary intent, it does not need to be represented.
3. Equivalent SQL, PromQL, and future-language queries should produce the same intent shape.

## Core nodes

### `Scan`

Identifies the logical data source.

```text
Scan(
    source = "metrics"
)
```

PromQL metric selection and SQL `FROM` clauses can both map into `Scan` when they denote
the same logical data domain.

### `Filter`

Restricts the logical input using predicates.

```text
Filter(
    input = Scan("metrics"),
    predicate = region = "us-east"
)
```

Filters are first-class because they can change which summaries are applicable. A summary
for an entire dataset is not necessarily sufficient to answer the same intent under an
arbitrary predicate.

### `TimeWindow`

Represents a semantic time restriction.

```text
TimeWindow(
    input = ...,
    window = [now() - 5m, now()]
)
```

A time window is explicit because time is a major part of PromQL semantics and may also be
material to summary selection and retention.

The representation may be absolute or relative, but it should be canonicalized so that
language-specific range syntax does not leak into L2.

### `Aggregate`

Represents grouped or ungrouped measurement of the input.

```text
Aggregate(
    input = ...,
    group_by = [FieldRef("service")],
    measures = [Count]
)
```

`Aggregate` is a central node because it exposes exactly the structure needed to choose
summary implementations.

#### Grouping dimensions

`group_by` contains semantic `FieldRef`s, not schema positions.

```text
group_by = [FieldRef("service"), FieldRef("region")]
```

#### Measures

Measures describe what statistic or aggregate is required. At minimum the algebra should
support the following semantic measures:

```text
Count
Sum(field)
Min(field)
Max(field)
Avg(field)
Quantile(field, q)
DistinctCount(field)
```

Additional measures can be added when there is a stable semantic distinction and a
meaningful summary implementation.

### `TopK`

`TopK` is a first-class semantic intent, even though some source languages express it as a
combination of sorting and limiting.

```text
TopK(
    input = ...,
    key = FieldRef("service"),
    measure = Count,
    k = 10
)
```

The reason to make it first-class is that top-k directly corresponds to summary families
such as heavy-hitter sketches. Keeping it as `Sort + Limit + Aggregate` would hide the
summary-relevant intent from L3.

### `Expression`

Represents arithmetic or semantic composition that does not yet justify a dedicated node.

```text
Expression(
    op = Divide,
    args = [
        Sum(FieldRef("errors")),
        Sum(FieldRef("requests"))
    ]
)
```

Expressions should remain deliberately generic. A new first-class node is justified when
an operation has independent semantic meaning for summary selection or correctness.

### `Join` (future / optional)

Joins should not be required by the first version of the algebra. They become necessary only
if supported query languages and target workloads require cross-source or cross-relation
intent.

If introduced, `Join` should be defined semantically rather than copied directly from SQL's
full join grammar.

---

## What is intentionally *not* a core L2 node

The following should generally be canonicalized away unless a specific summary-planning
requirement proves they need to survive:

// TODO: Why is sort not a node? What if a query has sort without limit? What if a query has limit without sort?

```text
Project
Sort
Limit
Alias
language-specific syntactic wrappers
```

For example:

```text
ORDER BY COUNT(*) DESC LIMIT 10
```

should normally become `TopK`, not remain as a generic `Sort` followed by `Limit`.

---
