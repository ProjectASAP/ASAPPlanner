# Shape-aware ERP v1

Audience: planner and evaluation developers.

## Contract

Sketch-bench remains the measurement authority. Each ERP record carries a
canonical workload descriptor plus `erp_shape = {cardinality, zipf_exponent,
benchmark_events}` and atomic seconds per update, merge, and query. `null`
Zipf exponent means uniform; it is not interchangeable with Zipf.

The runtime reports an observed shape. Planner chooses the nearest benchmark
shape only inside explicit cardinality/skew distance bounds and only when the
benchmark event count passes a sufficiency floor. Event count is deliberately
not a nearest-neighbor axis after that floor: more samples from the same
stationary distribution should not make a profile semantically farther away.
No candidate inside the bounds means profile miss, so Hybrid callers retain
their theoretical-sizing or Exact fallback.

## Window cost composition

Benchmark CPU numbers are atomic unit costs. For a pane plan:

```text
updates = input_updates * materializations
merges = query_executions * (panes_per_query - 1)
queries = query_executions
retained_sketches = retained_panes * materializations
cpu = updates*C_update + merges*C_merge + queries*C_query
```

A tumbling window has one pane per query and therefore zero merge operations.
A shared sliding-window plan has one materialization; a natural per-query
deployment has one materialization per distinct window. Memory is computed from
retained sketches and the measured bytes per sketch, outside the CPU equation.

## Safety boundary

Nearest matching is empirical evidence, not a theoretical accuracy guarantee.
Distribution-family mismatch, insufficient benchmark volume, excessive shape
distance, absent metrics, unsupported runtime parameters, and drift all fail
closed. Bursts are benchmark scenarios/provenance in v1; the online controller
may select a burst-specific profile only when that scenario is explicitly
declared rather than silently interpolating it.
