## Parse

This steps parses a query workload and converts it to the pre-ASAP IR. This is implemented using:
(1) Language-specific parser (often an external dependency)
(2) Language-specific component that convers the parsed AST into a pre-ASAP plan

Examples:

```text
PromQL topk(10, count by (service) (...))
    -> TopK(k=10, key=FieldRef("service"), measure=Count, ...)
```

```text
SQL ORDER BY COUNT(*) DESC LIMIT 10
    -> TopK(k=10, key=..., measure=Count, ...)
```

## Canonicalize

`canonicalize` normalizes semantically equivalent intent trees so that equivalent queries
from different languages, or differently phrased queries within one language, converge on
the same canonical shape.

> Canonicalization is allowed to remove syntax that has no independent semantic meaning for
summary planning. For example:

```text
Aggregate -> Sort(desc) -> Limit(k)
```

can canonicalize into:

```text
TopK(k, key, measure, Aggregate(...))
```

when the ordering expression and limit form the semantics of a top-k request.

>  Canonicalization should be driven by **semantic equivalence and summary relevance**, not by
trying to reproduce every relational operator in a universal AST.
