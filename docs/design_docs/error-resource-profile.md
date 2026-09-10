# Error–Resource Profile (ERP)

## Status

ERP v1 is a discrete, shape- and distribution-conditioned profile exchanged between
`sketch-bench` and ASAPPlanner. It is an empirical planning input, not a proof
of a worst-case sketch guarantee. The implementation lives in
`asap-aware-mapping::erp`; `approxbench erp` exports the producer artifact.

## Motivation

Closed-form sketch bounds are intentionally distribution-independent and can
therefore over-allocate state for stable production distributions. A benchmark
measurement indexed only by `(algorithm, parameters)` has the opposite bug: it
can silently reuse a favorable result for a different distribution. ERP keeps
error, resources, configuration, and the measured distribution together.

ERP is analogous to an Error–Latency Profile, but exposes all resources needed
by a materialization planner:

```text
(sketch, parameters, distribution, implementation)
    -> (observed error metrics, update/merge/query CPU, retained bytes)
```

ASAPPlanner combines the profile with recurrence, retention, and window plans;
sketch-bench does not decide whether or where a summary is materialized.

## Ownership and data flow

```text
sketch-bench parameter/distribution sweep
    -> flat MergedRecord rows
    -> `approxbench erp`
    -> versioned ERP artifact
    -> backend observes live cardinality/distribution shape
    -> deployment supplies an exact or nearest ErpSelectionRequest
    -> ASAPPlanner filters applicable and accurate records
    -> least estimated workload cost
    -> deployment maps the selected identity to runtime configuration
```

The benchmark owns measurements and the exact workload descriptor. The planner
owns validation and selection. A deployment owns query-to-error-metric mapping,
current distribution observation, resource weights, runtime capability checks,
and the final formal/empirical/exact policy.

## ERP v1 record

Each record contains:

- an artifact-local record ID and producer revision;
- structural sketch and implementation identities;
- the exact construction parameters that ran;
- the complete synthetic generator description or external trace descriptor;
- the number of trials;
- named observed error metrics; and
- memory plus per-operation update, merge, and query CPU.

The canonical workload remains available as an opaque wire value for exact
matching. Shape-aware records additionally carry:

```text
erp_shape = {
  cardinality,
  family,              // uniform | zipf | power_law | normal | empirical | ...
  parameters,          // family-specific numeric parameter map
  benchmark_events
}
```

The family is explicit rather than encoding uniform as a missing Zipf
parameter. Profiles from different families are never interpolated. Parameter
keys must match before a distance is computed; this keeps the contract open to
Zipf exponent, continuous power-law alpha/minimum, normal mean/deviation, and
future synthetic or fitted families. Empirical/custom traces carry a stable
family and descriptor and normally use exact matching rather than synthetic
interpolation. `benchmark_events` is a sufficiency gate, not a distance axis.

## Selection

Given applicable record `r` and request `q`, v1 accepts `r` iff:

```text
r.distribution == q.distribution
implementation matches, when constrained
sketch is in the deployment capability set
r.trials >= q.min_trials
r.error_metrics[q.error_metric] <= q.max_error
```

The estimated workload cost is:

```text
cpu_weight * (
    expected_updates * update_cpu_seconds
  + expected_queries * query_cpu_seconds
  + expected_merges * merge_cpu_seconds)
+ byte_second_weight * retention_seconds * memory_bytes
```

For shape-aware selection, records must first satisfy the benchmark-event floor
and distribution-family constraint. Their normalized distance is the maximum
of log2-cardinality distance and every family-specific parameter distance. Only
candidates within all caller-supplied bounds are eligible; cost selects among
those candidates.

The least-cost accepted record wins. Missing error metrics, missing contexts,
invalid values, and insufficient trials make a record inapplicable. Cost ties
are broken by record ID; producers should avoid duplicate physical points.

This is a bounded discrete search over measured points. Unlike AutoSketch, ERP
does not run LHS and neighbor benchmarks during query planning. Profiling cost
is paid offline and reused across planning cycles.

## Accuracy modes

Deployments must expose the selected guarantee kind to users.

### Formal

Use theoretical sizing. ERP may estimate resource cost or rank families, but
must not reduce parameters below the formal minimum. This mode is outside the
empirical selector because its feasibility evidence comes from the analytical
accuracy model.

### Empirical

ERP may select a parameter point smaller than the theoretical minimum. The
claim is limited to the matched distribution, query error metric, implementation,
and measured population. It must never be serialized as a formal `(epsilon,
delta)` guarantee.

### Hybrid

Try empirical selection. If no applicable ERP record exists, evidence is stale,
or runtime drift invalidates the context, the deployment falls back to formal
sizing or exact execution. V1 returns `NoApplicableConfiguration`; the caller
performs this fallback explicitly so it cannot be mistaken for a measured zero.

## Window cost composition

The benchmark provides atomic unit costs. Planner derives operation counts from
the selected materialization and window model:

```text
updates = input_updates * materializations
merges = query_executions * (panes_per_query - 1)
queries = query_executions
retained_sketches = retained_panes * materializations
cpu = updates*C_update + merges*C_merge + queries*C_query
```

A tumbling window has one pane per query and no merge. A shared sliding-window
plan has one materialization; a natural per-query deployment has one per
distinct window. Retained memory is measured bytes per sketch multiplied by
retained sketches, outside the CPU equation.

## Distribution drift

ERP selection is valid only while the observed shape remains inside the
configured profile distance. The deployment periodically derives a signature
and triggers replanning on mismatch. Insufficient benchmark volume, excessive
shape distance, absent metrics, unsupported runtime parameters, and drift all
fail closed. Bursts are explicit benchmark scenario provenance and are not
silently inferred or interpolated.

Recommended future signatures are family-specific:

- CMS: cardinality, entropy, heavy-hitter mass, and L1 frequency;
- CountSketch: F2/L2 tail energy;
- HLL: cardinality and duplicate rate;
- KLL/DDSketch: value range, target-quantile density, and tail shape; and
- Top-K: the frequency gap around ranks `k` and `k+1`.

## Statistical requirements

V1 transports comparator metrics already emitted by sketch-bench and enforces a
minimum trial count. Before making a probabilistic production SLA claim, a later
version must carry raw independent-trial errors or a declared confidence bound.
Mean error alone is not sufficient for a tail guarantee. Calibration and test
traces must remain disjoint in evaluation.

## Relationship to existing offline evidence

`EmpiricalEvidenceProvider` performs exact `(algorithm, parameters, distribution,
environment)` lookup and preserves formal sizing. ERP adds selection across
multiple measured parameter points and a recurrence/retention-aware resource
objective. The two paths intentionally coexist:

- existing evidence is the conservative formal-compatible path;
- ERP is the explicitly empirical or hybrid parameter-selection path.

Deployments should not silently enable ERP for users who requested formal
accuracy.

## Evaluation contract

Compare Theory, AutoSketch, ERP, and an exhaustive measured Oracle using the
same sketch implementation, parameter grid, calibration traces, test traces,
hardware, and error metric. Report selected bytes, accuracy violation rate,
optimality gap, planning time, total profiling cost, and behavior under
distribution shift. A second experiment may add ASAPPlanner recurrence/window
selection, but must identify that as capability beyond query-local sizing.

## Future work

- richer family-specific signatures beyond cardinality and Zipf exponent;
- confidence/quantile error summaries from independent trials;
- environment descriptors and validity intervals in the ERP wire format;
- multi-state error composition for merged windows and query DAGs;
- Pareto-front compression of dominated profile points; and
- online feedback that creates a new immutable profile generation rather than
  mutating historical benchmark evidence.
