# Native PromQL source rows

Audience: source-adapter and physical-executor developers.

A PromQL query only names some labels. Those columns cannot establish series
identity for Rate or TopK: two series with the same `job` may have different
unreferenced instance labels.

`physical_planner::promql_rows::with_series_identity` resolves supported unary
PromQL computations to a bounded row representation before candidate search.
It appends `$promql_series_identity`, a non-null UTF-8 column containing the
canonical JSON encoding of the full label map. The name cannot collide with a
legal PromQL label. The resulting schema is closed over physical columns; the
label map remains dynamic and is not restricted to labels named in the query.
This realization rejects unsupported label rewriting, implicit vector matching,
and `without` operations rather than dropping hidden labels.

Source adapters construct batches with `series_row`. Named label columns are
projections of the same complete identity; absent named labels project to empty
strings. `decode_series_identity` restores all labels on result conversion and
rejects noncanonical encodings. A query adapter must still apply the selected
operator's metric-name/result-label rules. Source selection, complete window
coverage and revision admission remain deployment responsibilities.

Planner's maintained-population candidate recognizes this explicit identity
representation. Its TopK readout compiles automatically to `CurrentSeries`,
`Sort`, and `Limit`; deployment supplies the raw boundary or an already maintained
population boundary. Compilation does not open either source.

The native `CurrentSeries` operator selects the latest sample per complete
identity in `(evaluation_time - lookback, evaluation_time]`. It removes stale
markers after selecting the latest sample, so an older value cannot reappear.
It rejects conflicting values at one series timestamp and emits the evaluation
timestamp. Each run builds a new snapshot; decreased values and expired series
cannot retain earlier heap weights. It reserves workspace and observes the
run's cancellation and byte budget. Precompute scopes must match the declared
lookback before any input is polled.

CMS/CountSketch heap operators can consume this snapshot. CMS still requires
nonnegative weights; legal approximate TopK admission still requires the
Planner's accuracy/membership evidence. Executing a heap does not establish
that its result satisfies a query's accuracy requirements.

Tests cover open-label Rate → CMS/CountSketch heaps, hidden-label round trips,
reset and zero-rate cases, snapshot replacement/decrease/expiry/staleness,
serialized physical recovery, and resource rejection. These are shared-library
tests, not proof of Backend candidate selection or durable deployment execution.

Spatial heap candidates use the same complete series identity. Planner's
`current_series_topk_candidates` explores a CountSketch-with-heap realization
of canonical Sort/Limit under an explicit accuracy target. The physical DAG
selects the latest eligible samples before building a fresh heap. A maintained
population boundary can supply that snapshot directly. Arbitrary signed metric
values do not authorize CMS; counter Rate's non-negative proof is separate.
These candidates still require membership/score evidence for deployment admission.
