# Physical compile coverage for deployment computation

Audience: developers moving computation from ASAPQuery-backend into
`asap_physical_operators::physical_planner`.

## Contract

Logical selection decides what to compute. The maintenance lifecycle sets node
timing. `physical_planner::compile` turns a timed `PostAsapDag` into physical
operator DAGs. The backend owns ingestion, panes, storage, stored-state
readout, external exact engines, pricing/selection, and execution scheduling.

A backend lowering is *covered* when `compile` accepts the corresponding
`PostAsapDag` node and produces operators with the same result. The backend
should then pass the timed DAG and its input contracts to `compile`. It should
not rebuild operator choices from PromQL text or construct operators itself.

## Inventory

Surveyed backend: `ASAPQuery-backend` branch `perf/788-startup-search`.
Planner base: `split/462-f-physical-planner` (#475).

Status values:

- **Supported**: `compile` or `compile_node` already covers this computation.
- **Partial**: some shapes are covered. The Notes column lists the gap.
- **Missing**: `compile` rejects this computation.
- **Backend**: not computation, or owned by the backend.

| # | Backend site | Computation | Planner node | Status at #475 | Notes |
|---|---|---|---|---|---|
| 1 | `query_time.rs` `Lower::lower`, `compile_logical` | PromQL AST → `QueryTimeOperator` graph for a native query | `Fallback { QueryExpr }` subtrees plus value payloads | Missing | `compile` lowers `Fallback` only as a raw `Scan` source. |
| 2 | `QueryTimeOperator::Aggregate` (sum/min/max/avg/count) | Grouped value aggregation | `Value::Exact(Aggregate)`; `SummaryAgg{ExactAggregate, Reduce}` over finalized values | Supported | Also `promql_values::compile_aggregate`. |
| 3 | `QueryTimeOperator::Sort`, `Limit` (topk, sort, sort_desc) | Ordering and per-group limits | `Value::Sort`, `Value::Limit` | Supported | |
| 4 | `QueryTimeOperator::Binary`, `QueryPlanNode::Binary` (vector ⊗ scalar) | Arithmetic with a scalar operand | `Binary` whose operand is `Fallback{PromqlScalarBridge(Literal)}` | Missing | Query-time `Binary` accepts only label-map vector schemas. The literal node has no native binding. |
| 5 | `QueryTimeOperator::Binary` (vector ⊗ vector) | One-to-one label matching and arithmetic | `Binary` over grouped value rows | Missing | Only the ingestion-time `aligned_binary` and label-map `vector_binary` exist. |
| 6 | `binary_operator` CheckedDiv / FiniteDiv | Guarded division | `BinaryOperator` checked flags | Partial | Flags are evaluated, but only where rows 4/5 are covered. |
| 7 | `QueryTimeOperator::Binary` comparisons, `bool` | Filter or 0/1 comparison | `Binary{Compare}` | Missing | `Payload::Binary` does not carry `return_bool`. |
| 8 | `QueryTimeOperator::UnaryNegate` | Negation | `Binary{Mul}` by literal `-1` | Missing | The frontend emits `* -1`; same gap as row 4. |
| 9 | `QueryTimeOperator::VectorToScalar` | `scalar()` | `Fallback{PromqlScalarFromVector}` | Missing | Only `promql_values::compile_vector_to_scalar`. |
| 10 | `QueryTimeOperator::HistogramQuantile` | Bucket interpolation | `Fallback` / `AggIntent::HistogramQuantile` | Missing | Only `promql_values::compile_histogram_quantile`. |
| 11 | `QueryTimeOperator::Temporal` (rate, increase, `*_over_time`) | Per-series window functions | `SummaryAgg{PerEntity}` over `TimeRange(Scan)` | Partial | Supported with closed series identity. Not supported over `Fallback` matrices (`compile_temporal` only). |
| 12 | `logical_dag.rs` `Subquery`, `subquery_grid`, `expanded_inputs` | Re-evaluate the child on a step grid and assemble a matrix | `Fallback{PromqlSubquery}` | Missing | No Planner operator. |
| 13 | `QueryPlanNode::Scalar`, `DagCompiler::lower` scalar literal | Scalar constant | `Fallback{PromqlScalarBridge(Literal)}` | Missing | Only `promql_values::compile_scalar`. |
| 14 | `DagCompiler::lower` `ReduceSum`; `physical_values.rs` PerEntity projection | Sum over finalized values; per-entity identity | `SummaryAgg{ExactAggregate(Sum)}` | Supported | The backend builds an identity `Operator::project` itself for PerEntity. |
| 15 | `DagCompiler::lower` `ExactReadout`; `post_asap_readout.rs` ExactReadout | Finalize exact state (sum/count/min/max/rate/increase) | `Value::FinalizeExactAccumulator` | Partial | Count yields Int64 against a declared Float64 PromQL value. `compile` rejects it. |
| 16 | `post_asap_readout.rs` SummaryEstimate (`readout_bound`, `expand_item_rows`) | Sketch estimate per group; TopK item expansion | `SummaryEstimate` | Partial | The backend's label-map state layout and MetricsQL `__name__` rules have no Planner equivalent. `compile_exact_readout` has no sketch counterpart. |
| 17 | `post_asap_readout.rs` SummaryMerge (`merge_bound_states`) | Merge states by group | `SummaryMerge` | Supported | Union plus `summary_merge`. |
| 18 | `post_asap_readout.rs` counter range parameters | Counter lookback for rate/increase | `TimeRange` ancestor of finalization | Supported | Applied through `with_counter_lookback`. |
| 19 | `post_asap_readout.rs` `execute_value_fragment` | Per-timestamp binding of a value fragment | n/a | Backend | Evaluation scheduling. |
| 20 | `DagCompiler::lower` SummaryJoin / Subtract / Delete | Summary algebra | `SummaryJoin`, `SummarySubtract`, `SummaryDelete` | Missing | The backend also rejects these (`ExactFallback`). |
| 21 | `current_series.rs` Snapshot + TopK | Current-series ranking | `ReadPopulation{TopK}` | Supported | |
| 22 | `current_series.rs` Sum / Count / Average | Current-series aggregates | `ReadPopulation{Sum,Count,Average}` | Missing | `compile` accepts only TopK. |
| 23 | `current_series.rs` Quantile | Current-series quantile | `ReadPopulation{Quantile}` | Missing | No exact quantile reduction. |
| 24 | `raw_dag.rs` weight `Column` | Summary update from a sample/projected value | `SummaryAgg` | Supported | |
| 25 | `raw_dag.rs` weight `Constant` | Unit/constant-weight update | `SummaryAgg` | Missing | `compile_node` requires a column weight. |
| 26 | `raw_dag.rs` item `Column` / `Tuple` | Keyed update item | `SummaryAgg{item}` | Supported | `keyed_summary_build`. |
| 27 | `raw_dag.rs` item `EntityIdentity` | Series-identity item | `SummaryAgg{item}` | Missing | Needs the series-identity column. |
| 28 | `physical_values.rs` `compile`, `combine` | Translate `QueryTimeOperator` to `promql_values::*`; compose fragments | n/a | Supported | Exists only because of row 1. `CompiledPhysicalDag::compose` is Planner API. |
| 29 | `query_plan.rs` `compile_native_fragment` (Semi join, Exact aggregate, Sort, Limit, Filter) | Relational value ops | `RelationalJoin`, `Value::*` | Supported | Already calls `compile`. |
| 30 | `query_time.rs` `selected_query_time_nodes`, `selected_native_expression`, `selected_aggregate_operator` | Recover operator identity from original PromQL text | Payload variants (`ExactKind::Min`/`Max`, `AggIntent`) | Supported | Payloads already carry the identity. These witnesses are needed only while row 1 remains. |
| 31 | Scan, ExactSubquery, CandidateExactSubquery, CurrentSeries ingest, ReadMaterialization, ExternalExact | Storage reads and external engines | Input contracts | Backend | |

Totals at #475: 11 Supported, 4 Partial, 14 Missing, 2 Backend.

## Covered after this change

| Row | Change |
|---|---|
| 4, 8, 13 | Query-time `Binary` folds a scalar-literal operand into a projection over grouped value rows. |
| 5 | Query-time `Binary` over grouped value rows performs an inner equi-join on equal label columns, then applies the operator. Per-series rows remain Partial. |
| 15 | Count finalization converts exactly to the declared Float64 value. |
| 22, 23 | `ReadPopulation` Sum/Count/Average/Quantile compile to grouped aggregation. `Reduction::Quantile` implements PromQL interpolation. |

Totals after this change: 17 Supported, 4 Partial, 8 Missing, 2 Backend.

## Covered by PromQL fallback compilation

`compile` now lowers a `Fallback{QueryExpr}` node from its typed expression,
realized with `promql_rows::with_series_identity`. The deployment supplies the
raw rows of the `i`th selector returned by `promql_fallback::raw_series` at
`promql_fallback::raw_series_input(node, i)`, with that selector's schema. Each
selector has its own slot, even when two selectors read the same metric. The
node's own ID still names its output, so a deployment may instead supply the
whole result, for example from an external exact engine. A Fallback that reads
no selector, such as `vector(1)`, needs no input.

`Operator::series_window` evaluates each series at the query time, or at each
subquery step, over the left-open window `(t - offset - range, t - offset]`.
Instant selection takes the latest sample and omits the series if that sample is
a stale marker. Range functions ignore stale markers. The raw rows must cover
every window the node evaluates; `raw_series` documents the subquery extent.
A subquery has at most 100000 steps. Output rows keep the full series identity;
the query adapter still applies PromQL's metric-name rules. A bare selector
consumed by another node, such as a per-entity summary, remains raw range rows
and is not compiled as instant selection.

| Row | Change |
|---|---|
| 1 | Supported shapes: selectors with `offset`; `rate`, `increase`, `delta`, and `sum`/`avg`/`min`/`max`/`count_over_time`; `by` aggregation; `sort`; `topk`/`limit`; `scalar()`; `vector(literal)`; arithmetic with one literal. Now Partial. |
| 9 | `scalar()` compiles to `VectorToScalar`. |
| 11 | Range functions over the raw selector rows. |
| 12 | `f(sel[R:S])` and `f(g(sel[r])[R:S])`, with subquery `offset`, evaluate on the aligned step grid. The frontend now retains subquery `offset`/`@` as a `TimeShift`; it previously dropped them. Now Partial. |

Totals after this change: 19 Supported, 5 Partial, 5 Missing, 2 Backend.

## Covered by multi-selector fallback compilation

| Row | Change |
|---|---|
| 1 | Vector-vector arithmetic with one-to-one matching, `on`, and `ignoring`. Each side is reduced to its matching labels, then matched on equal label sets. A duplicate match group is an error, as in Prometheus. The result drops `__name__`. `without` aggregation. `irate`, `idelta`, `changes`, `resets`, `last_over_time`, and exact `quantile_over_time`. `@ <timestamp>` on selectors. Still Partial. |
| 11 | The range functions above, over raw selector rows. |
| 12 | `@ <timestamp>` on subqueries anchors the step grid. An inner selector's `@` pins every step. Still Partial. |

`@` fixes the instant a window ends at, before `offset`; the output keeps the
query's evaluation time. The raw rows must cover the window at that instant.
Vector matching and `without` rewrite the series identity, so their results
already lack `__name__`. Other results keep it; the query adapter still drops
it. (Range functions drop it too since the comparison change below.)

Totals are unchanged: 19 Supported, 5 Partial, 5 Missing, 2 Backend.

## Covered by per-series arithmetic

| Row | Change |
|---|---|
| 5 | Query-time `Binary` over rows with a series identity, such as per-series readouts of stored state, uses the Fallback's `series_labels` and `series_binary`. Examples: `avg_over_time` as stored sum/count, and `rate(a) / rate(b)`. Matching drops `__name__` and honors `on`/`ignoring` when the payload carries them. Only one-to-one arithmetic is covered; `group_left`/`group_right` stay rejected and comparisons are row 7. Now Supported. |
| 4, 8 | A literal operand also applies to per-series rows and drops `__name__`, in the Fallback too. Series whose label sets become equal are an error, as in Prometheus. |

Grouped `sum`/`avg`, current-series `Sum`/`Average` readouts, and
`sum_over_time`/`avg_over_time` use Prometheus' Kahan-Neumaier summation. An
average switches to an incremental mean once the running sum would overflow.
The grouped path also serves SQL `SUM`/`AVG` over Float64, which are now
compensated the same way.
Stored exact `Sum` state still sums without compensation, because a
compensation term would change the stored state layout. Its checked
`avg_over_time` division therefore fails instead of returning a finite mean.

Totals after this change: 20 Supported, 4 Partial, 5 Missing, 2 Backend.

## Covered by comparisons and set operators

The IR now distinguishes `bool` comparisons: `BinaryOpKind::CompareBool(op)`
beside the filtering `BinaryOpKind::Compare(op)`. The PromQL frontend emits it
for `bool`; it previously dropped the modifier.

`Operator::series_binary` evaluates every PromQL binary operator, in the
Fallback and in query-time `Binary` nodes, following Prometheus'
`VectorBinop`, `VectorAnd`, `VectorOr`, and `VectorUnless`:

- Arithmetic drops `__name__`. A comparison filter keeps the matched left
  series with its value and name; with a scalar on the left it keeps the
  vector's value. `bool` yields 1 or 0 and drops the name. NaN compares
  unequal to everything.
- Operands may be vectors, literals, `scalar()`, or scalar-valued binary
  expressions such as `-scalar(x)`. Two scalars yield a scalar. The
  label-map `vector_binary` also treats `CompareBool` as its `bool` mode.
- One-to-one matching and `group_left`/`group_right` with included labels.
  The "one" side must not repeat a match group; many-to-one results must be
  unique. A left duplicate is an error only if more than one match is kept.
- `and`, `or`, and `unless` match label sets many-to-many, with `on` or
  `ignoring`, and return the original series.
- Range functions other than `last_over_time` now drop `__name__` in the
  Fallback. Series whose label sets become equal are an error, as in
  Prometheus. Vector-scalar results are checked the same way. Inside a
  subquery the inner function also drops the name, without that check,
  because each series repeats across steps.

A result is written in the left operand's schema. Without a series identity,
its label columns must hold every label the right side can contribute
(`or`, `group_right`, and `group_left` labels); otherwise `compile` rejects it.
So `sum(a) or vector(0)` compiles, but
`sum by (job) (a) * on(job) group_left(team) info` is rejected: the
aggregate's schema has no `team` column.

| Row | Change |
|---|---|
| 1 | Comparisons, `bool`, set operators, `group_left`/`group_right`, `scalar()` operands, and literals over aggregates whose value has another name, such as `sum by (job) (a) * 2`. Still Partial. |
| 5 | Grouped `Binary` rows use the same operator instead of a relational join. A duplicate match group is now an error instead of a cross product. |
| 7 | Fallback, grouped `Binary`, and per-series `bool` comparisons. Per-series filter comparisons and set operators on `Binary` nodes fail closed: stored readouts keep `__name__` even where the range function drops it. Now Partial. |

Totals after this change: 20 Supported, 5 Partial, 4 Missing, 2 Backend.

## Covered by classic histogram_quantile

| Row | Change |
|---|---|
| 10 | A classic-bucket `histogram_quantile(q, v)` lowers to `Aggregate{Reduce(without([le])), [HistogramQuantile{q, le}]}`, where `le` is the argument's `le` column. The frontend seeds `le` into the selector's schema. The Fallback compiles it to one operator that groups rows by every label except `le` and applies Prometheus `bucketQuantile`. The output labels are the input labels without `le` and `__name__`; result label sets that become equal are an error, as in Prometheus. Covers `histogram_quantile(q, rate(x_bucket[5m]))`, `histogram_quantile(q, sum by (le, job) (…))`, and bare bucket selectors. Now Supported. |

An argument whose output provably lacks `le`, such as
`sum by (job) (rate(x_bucket[5m]))`, is rejected at lowering. Prometheus
returns an empty vector for it. Candidate search keeps the classic form as one
exact `KeepPreAsap` subtree for every accuracy target; it has no sketch
candidate. `histogram_quantiles` lowers each branch the same way, but the
Fallback compiler does not yet accept its `Concat` of relabeled branches.

Totals after this change: 21 Supported, 5 Partial, 3 Missing, 2 Backend.

## Remaining

In order of backend usage:

1. Rows 1 and 12, the remaining `Fallback` shapes:
   - `time()` and other scalar functions as operands.
   - Subquery operands other than one per-series function; implicit
     subquery resolution, which is a deployment default.
   - `@ start()` and `@ end()`, which need the range query's bounds in the
     run scope.
   - Other functions, such as `deriv`, `predict_linear`,
     `stddev_over_time`, `absent`, `label_replace`, and math functions.
   After these shapes are covered, the backend can delete rows 28 and 30.
2. Row 7: per-series readouts must drop `__name__` where their range
   function does, and check for equal label sets. Then filter comparisons
   and set operators over them can compile.
3. Rows 25 and 27: constant weights and `EntityIdentity` items for precompute
   `SummaryAgg`.
4. Row 16: a label-map sketch-state readout, the counterpart of
   `compile_exact_readout`, and MetricsQL `__name__` retention rules.
5. Row 20: summary join, subtract, and delete.
6. Compensated stored exact `Sum` state, a state-layout change shared with the
   backend's stored-state decoding.
7. Non-finite literals (`NaN`, `Inf`) in compiled operators do not survive a
   JSON round trip of the program.
8. `group_left` labels and `or`/`group_right` right-side labels onto
   aggregated (label-column) rows. The logical output schema, which is the left
   side's, has no column for them.
9. An equal-label-set check for inner subquery functions, per step.

`fill`, `fill_left`, and `fill_right` matching modifiers are rejected by the
frontend (#494); they are never silently ignored.
