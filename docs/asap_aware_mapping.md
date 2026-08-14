ASAP-aware mappings decides **whether and how an intent can be answered by a summary** rather than by scanning
raw data.

This is the layer where concrete summary families, parameters, and implementation strategies
are selected.



```text
L2 Intent
    │
    │ implement
    ▼
Summary-bound IR
```

An implementation is the concrete realization chosen for an intent, for example:

```text
summary family + parameters
exact accumulator
PassThrough (compute from raw data)
```


## Summary mapping

Representative mappings include:

```text
Count
    -> exact counter / count summary

DistinctCount(field)
    -> HyperLogLog-family summary

Quantile(field, q)
    -> quantile sketch family such as KLL / DDSketch

TopK(key, Count, k)
    -> heavy-hitter family such as SpaceSaving
```

The mapping is not necessarily one-to-one. Multiple summary candidates may satisfy one
intent, and a cost model may rank those candidates.

For example:

```text
Quantile(latency, 0.99)
        ↓
Candidate summaries:
    KLL
    DDSketch
    exact accumulator
        ↓
Cost / accuracy / latency constraints
        ↓
chosen implementation
```

## Intent-driven summary selection

The summary selector should reason over the **shape of the intent**, not over source-language
syntax.

For example, both:

```text
PromQL: topk(10, count by (service) (...))
SQL: ORDER BY COUNT(*) DESC LIMIT 10
```

arrive at:

```text
TopK(..., Count, ...)
```

and therefore reach the same summary-selection logic.
