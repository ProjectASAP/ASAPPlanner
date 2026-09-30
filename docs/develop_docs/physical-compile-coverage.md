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

`compile` now lowers a `Fallback{QueryExpr}` node from its typed expression.
The expression must read at most one selector, realized with
`promql_rows::with_series_identity`. The deployment supplies that selector's raw
rows at `promql_fallback::raw_series_input(node)`, with the schema returned by
`promql_fallback::raw_series`. The node's own ID still names its output, so a
deployment may instead supply the whole result, for example from an external
exact engine. A Fallback that reads no selector, such as `vector(1)`, needs no
input.

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

## Remaining

In order of backend usage:

1. Rows 1, 10, and 12, the remaining `Fallback` shapes:
   - Subtrees with more than one selector, such as vector-vector binaries.
   - `histogram_quantile`: the frontend emits `by ()` grouping with no
     output labels. The IR must group `without (le)` and keep the labels.
   - Subquery operands other than one per-series function; implicit
     subquery resolution, which is a deployment default.
   - `@` on selectors and subqueries; `without` grouping; `irate`,
     `changes`, and other range functions.
   After these shapes are covered, the backend can delete rows 28 and 30.
2. Row 7: comparison filters and `bool` comparisons. This needs `return_bool`
   in the `Binary` payload. `compile` currently rejects comparisons.
3. Row 5 for per-series rows: matching needs a metric-name-free series
   identity, not the full `$promql_series_identity`.
4. Rows 25 and 27: constant weights and `EntityIdentity` items for precompute
   `SummaryAgg`.
5. Row 16: a label-map sketch-state readout, the counterpart of
   `compile_exact_readout`, and MetricsQL `__name__` retention rules.
6. Row 20: summary join, subtract, and delete.
