## Parse

This steps parses a query workload and converts it to the pre-ASAP IR. This is implemented using:
(1) Language-specific parser (often an external dependency)
(2) Language-specific component that convers the parsed AST into a pre-ASAP plan

Examples:

```text
PromQL topk(10, count by (service) (...))
    -> Aggregate(reduction=Reduce(by=[service]), measures=[TopK{k=10}], ...)
```

```text
SQL ORDER BY COUNT(*) DESC LIMIT 10
    -> Aggregate(reduction=Reduce(by=[...]), measures=[TopK{k=10}], ...)
```

## Canonicalize

`canonicalize` normalizes semantically equivalent intent DAGs so that equivalent queries
from different languages, or differently phrased queries within one language, converge on
the same canonical shape.

> Canonicalization is allowed to remove syntax that has no independent semantic meaning for
summary planning. For example:

```text
Aggregate -> Sort(desc) -> Limit(k)
```

can canonicalize into:

```text
Aggregate(reduction=Reduce(by=[key]), measures=[TopK{k}], ...)
```

when the ordering expression and limit form the semantics of a top-k request — the
heavy-hitter intent becomes an `AggIntent::TopK` measure on `Aggregate`, not a separate node.

A `Sort` and `Limit` partitioned by the same keys promote to a `TopK` grouped by those keys:
the top `k` rows per partition, as PromQL `topk by (city) (k, …)` gives.

### Per-partition top-k in SQL

SQL spells the per-partition top-k with a window function:

```sql
SELECT city, host, c FROM (
  SELECT city, host, COUNT(*) AS c,
         ROW_NUMBER() OVER (PARTITION BY city ORDER BY COUNT(*) DESC) AS rn
  FROM metrics GROUP BY city, host) WHERE rn <= 5
```

The frontend lowers it as written: `Project{city, host, c}` over `Filter{rn <= 5}` over the
derived table's `Project` over `SQLWindowFunc{RowNumber}` over the `Aggregate`. Canonicalize
rewrites that chain, when the outer SELECT list drops `rn` and keeps only columns of the
aggregate, to the per-partition `Limit{5}` over `Sort{COUNT(*) DESC}` over the aggregate,
both partitioned by `city`:

1. When the SELECT list keeps the partition and ordering columns, the result is
   `Limit{Sort{Project{SELECT list}{Aggregate}}}`, which the heavy-hitter promotion above turns
   into `Aggregate(Reduce(by=[city]), [TopK{5}])` over the `COUNT(*)` aggregate: the shape
   PromQL `topk by (city) (5, sum by (city, host) (count_over_time(metrics[5m])))` lowers to.
   An ordering by anything but one descending count or sum (e.g. `AVG(latency)`, or a second
   tie-breaking key) stays the generic partitioned `Limit{Sort}`.
2. Otherwise (e.g. `SELECT host, c`), the SELECT list stays on top:
   `Project{Limit{Sort{Aggregate}}}`.

`rn < k` is `rn <= k - 1` and `rn = 1` is `rn <= 1`. A visible `rn`, another window function or a computed SELECT item
keep the window. Rows tied on the ordering value at the k-th place are cut arbitrarily, as
`ROW_NUMBER` itself numbers ties in no particular order; an exact plan and a heavy-hitter sketch
may keep different ones of them.

### Rewrites that change a node's columns

The heavy-hitter promotion changes the node's output columns: `Limit{Sort{Aggregate}}` outputs
the aggregate's `[keys.., items.., value]`, `Aggregate([TopK])` its partition keys and
`topk_k`. A consumer reads its input's columns by position, so the promotion applies only to a
node no consumer reads that way: a query's root, or a branch of a `Concat` no consumer
reads that way either. Under any other consumer (`SELECT host FROM (… ORDER BY c DESC LIMIT 5)`, a join) the ranking stays
`Limit{Sort}`.

A top-k answers, after its `[keys.., topk_k]` columns, the columns naming each ranked item, in
an exact plan and from a heavy-hitter sketch alike: a PromQL top-k the chosen series' labels, a
SQL top-k the aggregate's other group keys (`host` above).

>  Canonicalization should be driven by **semantic equivalence and summary relevance**, not by
trying to reproduce every relational operator in a universal AST.
