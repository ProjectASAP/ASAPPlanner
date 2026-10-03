# Operator/scalar design acceptance for #528

Audience: maintainers reviewing the implementation of [#511](https://github.com/ProjectASAP/ASAPPlanner/pull/511).
Reviewed against the current proposal and upstream documentation on 2026-10-02.
This is a representation and semantic-contract review, not a claim that the native
executor implements every SQL or PromQL feature.

## Unified graph and migration

`LogicalDAG` and `LogicalASAPDAG` name planning stages, not different Rust graphs.
Both use `Rc<OperatorNode>` with `Operator::NonASAP(NonASAPOp)` or
`Operator::ASAP(ASAPOp)`, the same `Schema`/`Field`/`FieldDataType`, and shared
operator edges, including producers referenced by scalar expressions.
`QueryRoot::Scalar` owns a scalar expression without fabricating an operator.
There is no `KeepPreAsap` or `ScalarBridge` operator. `retain_exact` annotates an
unchanged ordinary sub-DAG; it does not wrap it. The viewer uses ordinary node
kinds in both stages. Historical `PostAsapDAG` names denote the flat executable
wire document (version 7), not another logical IR.

`validate_structure` permits unassigned timing, checks scalar scopes, predicates,
result kinds, leaf declarations, state families, and retained field types.
`validate_execution_timing` additionally checks assigned phases and dependencies;
it does not decide whether a backend supports an implementation. A regression
allows pointwise arithmetic at ingestion time. Existing lifecycle/deployment
policy remains separate work in [#520](https://github.com/ProjectASAP/ASAPPlanner/issues/520)
and [#530](https://github.com/ProjectASAP/ASAPPlanner/issues/530).

`PlanOutput` is one multi-root workload DAG: `operator_roots()` exposes operator
roots, and `operators()` inventories shared nodes once across operator and scalar
roots. Replacement regions and CSE use `SubDAG` and `share_common_sub_dags`.
Bulk retained-sub-DAG cost evidence may cover only ordinary operators; it is
rejected if any descendant is an ASAP operator, so summary work cannot be hidden.
Supporting operator parameters live in `ir::operator_properties`; schema derivation
and errors have dedicated modules:

| Module | Responsibility | Example |
|---|---|---|
| `ir::operator_properties` | Parameter types stored in operator payloads, rather than derived node metadata | `GroupKeys` for aggregation, `JoinKind` for joins, `WindowFrame` for SQL windows |
| `ir::aggregate_schema` | Compute output columns and types from input schema and aggregate reduction | Preserve grouping columns and derive the aggregate result column |
| `ir::error` | `SchemaDerivationError` from schema/type derivation; other validation errors remain separate | Invalid grouping-column index or scalar-function signature |

Summary operations use “evaluation”;
`SketchStatistic` specifies the statistic to compute, rather than another query.
Wire version 7 reflects these renamed serialized variants and fields. Regenerate
older exported graphs and native programs; no legacy-name aliases are provided.

`ASAPStrategies` proposes supported ASAP realizations, including exact
accumulators and sketches; its name does not restrict candidates to sketches.
The API, diagnostics, caller imports, and current documentation use this name.

## Document examples

| Example | Evidence |
|---|---|
| Batch `SUM(bytes) + 1`, `SUM(bytes) * 2` | `batch_planning_replaces_and_shares_summary_operators`: invokes the actual planner, selects one shared summary state across two roots, validates and natively executes both results (31 and 60 for inputs 10 and 20). |
| `SELECT l_quantity * 2 AS q2 FROM lineitem WHERE l_quantity > 10` | `integration-tests/tests/operator_design_examples.rs`: parse, resolve, validate and flat export; Int64 projection and Boolean predicate. |
| `SELECT SUM(bytes) + 1 AS total_bytes FROM requests WHERE status = 200` | Same suite: explicit summary build/finalize rewrite, identical nullable Int64 result schema, one exported node per operator. Native execution of the ordinary SQL plan covers filtered rows, empty input and all-NULL input. The logical rewrite does not imply native Int64 summary-kernel support. |
| `2`, `time()`, `up * 2`, `vector(time())`, `scalar(sum(up)) + 1` | `asap-physical-operators/tests/promql_fallback.rs::scalar_design_document_examples_execute`: parse through native compilation and execution with hand-computed values. `frontend-promql/tests/scalar_design.rs` checks scalar roots, expression ownership and producer sharing. |
| Unary/math expressions and nonconstant scalar parameters | Native `pointwise_projection_names_and_dynamic_parameters`: unary name retention, arithmetic/math name removal, `round(m, scalar(vector(2)))`, `clamp(m, time()-301, time())`, reversed clamp bounds and calendar functions. |

## Comparison-table review

The following groups cover the rows in the proposal's SQL and PromQL comparison
tables. “Represented” describes the typed IR, not blanket parser/runtime coverage.

| Table rows | Review result and regression evidence |
|---|---|
| SQL scan/filter/project, VALUES, constants, Boolean/NULL/CASE/casts | Represented. Predicates require Boolean types, columns must resolve, and Values rows match declared arity/nullability. DataFusion coercions are retained explicitly. `types/tests/structure_contract.rs`, `frontend-sql/tests/sql_lowering.rs`. |
| SQL unary minus, BETWEEN, IS TRUE, null-safe comparisons, scalar functions/NOW | Explicit scalar expressions; only registered function contracts receive types. Unknown functions no longer receive a placeholder Float64. NOW remains a timestamp context read. Existing SQL lowering tests cover these forms. |
| SQL joins and set/bag operations | Relation inputs and concatenated predicate scopes checked; existing SQL lowering tests cover outer null extension, semi/anti joins and ALL flags. Nullable NOT IN is retained explicitly, not rewritten to ordinary anti-join. |
| SQL grouping, computed aggregate arguments, COUNT(x), HAVING | Scalar input projection and per-measure filters are retained. Global and filtered SUM/AVG are nullable; SUM preserves integer type. Window SUM preserves its argument type, and MIN/MAX window outputs are nullable because frames may be empty. Existing count-filter/grouping tests and the SQL document-example suite cover these contracts. |
| SQL QUALIFY/DISTINCT ON, grouping sets, wildcard/name alignment, window functions, sorting/limits | Existing frontend lowerings and payload tests remain. ROW_NUMBER stays a window column followed by its filter; the old TopK rewrite discarded that column and broke outer positional references. ORDER BY/LIMIT promotion remains only for its existing additive-ranking contract. Parser syntax coverage remains that of the installed DataFusion version. |
| SQL scalar/EXISTS/IN subqueries, derived tables/CTEs | Scalar plan references participate in traversal/canonicalization/export. Scalar subqueries retain zero-row NULL and multi-row error requirements. Positive EXISTS/IN filter conjuncts may still use proven semi joins. Select-list subquery execution is not added to the native backend by this PR. |
| PromQL literals/time/scalar arithmetic/comparisons | Scalar roots and nested expressions; bool comparisons return numeric 0/1. No scalar operator result kind. Scalar conversions validate producer kinds. Scalar and native fallback tests. |
| PromQL selectors/ranges, vector arithmetic/comparison/set matching | Typed temporal and binary operators preserve lookback, staleness, matching, bool mode and operand order. Existing conformance, numeric, label-matching and native fallback suites remain. Mixed scalar/vector operations use Project/Filter and visible scalar dependencies. |
| PromQL grouping/ranking/range reducers | Existing intent, grouping and partitioned Sort/Limit contracts remain; exact named range functions are not replaced by ordinary SQL SUM. Constant-parameter limitation is explicit. |
| PromQL vector/scalar conversions and nested subqueries | Real kind/cardinality conversions retained. Nested grid/offset/anchor tests remain. A SQL relation cannot silently enter PromqlSubquery without a vector conversion. |
| PromQL math/date functions | Scalar FunctionCall within Project, including expression parameters. Unary Negative retains metric name. `timestamp(v)` remains a temporal sample-timestamp operation, unlike calendar functions over sample values. |
| Request-duration expressions; relabel/absence/info/sampling | Existing registered/parser-supported forms remain; request-context syntax unavailable in the installed parser is a frontend gap, not an invented scalar contract. Existing partial-support and rejection tests remain. |

## Explicit gaps and compatibility changes

The proposal's gap rows remain gaps: unsupported aggregate/window modifiers,
correlation/recursion, lambdas, general ANY/ALL, unbound parameters, unrepresented
value types, explicit pattern escapes, general table functions, dynamic aggregate
parameters, fill modifiers, and extended range/start-timestamp metadata. Rejection
may occur in the parser or the frontend; this PR does not upgrade either language
parser to the current upstream release.

Native histogram samples have no IR sample type. Declared native samples and
native-histogram functions are rejected rather than treated as ordinary floats.
Classic bucket interpolation remains supported. Generic histogram-quantile sketches
require the explicitly declared, nonstandard `HistogramKind::RawSamples` extension.
`histogram_metadata`, `promql_conformance` and `promql_lowering` test both paths.
The corpus now records 1121 lowered / 469 rejected / 233 parser gaps; the previous
floor counted unsupported histogram operations as successful float lowerings.

Registered ClickHouse parser stubs are not automatically typed scalar contracts.
The 200-query January corpus now records 105 lowered, 53 invalid representations,
41 planning errors and 1 unsupported feature. This is an intentional compatibility
change: unsupported signatures and invalid scopes fail instead of receiving dummy
types. The corpus remains fully exercised. Exact SUM state now records whether any
non-NULL value was observed; persisted older SUM payloads lacking that field must
be rebuilt (decoding fails closed).

## Upstream references and validation

Semantic references: [Prometheus operators](https://prometheus.io/docs/prometheus/latest/querying/operators/),
[Prometheus functions](https://prometheus.io/docs/prometheus/latest/querying/functions/),
[DataFusion scalar functions](https://datafusion.apache.org/user-guide/sql/scalar_functions.html),
and [DataFusion subqueries](https://datafusion.apache.org/user-guide/sql/subqueries.html).
The comparison is against these current contracts; dependencies remain pinned by
Cargo.lock. Tests use explicit expected results, not live upstream differential
execution. Review and implementation were performed by the same agent.

Validation commands: workspace tests, fmt, clippy with warnings denied, the external
MetricsQL consumer, and DAG viewer Python tests (24 passed; 6 requiring Node.js
skipped because Node.js is unavailable in this environment). The vendored MetricsQL baseline also passes on Rust 1.99 (the CI toolchain),
verifying its existing 21 library and 3 doctest failures. Rust 1.98 changes one
compiler-diagnostic fingerprint; no baseline hashes or vendored sources were
changed to accommodate that older toolchain. Formatting and clippy pass on 1.99.

## Planner-layering follow-up: mergeable state

`ASAPOp::SummaryMerge` now derives and checks its shared schema, preserves the
state family and parameters, and validates dependencies at either execution
phase. Empty merges, raw inputs and differing state/grouping schemas fail.
`types/tests/summary_merge.rs` checks these contracts and executable export.
`integration-tests/tests/planner_layering_merge.rs` builds five one-minute KLL
states, merges them through unified export/native compilation, and checks p99
for both rebuilding raw inputs and reading materialized panes (Examples 3B/4B).
This proves the merge building block; automatic window candidate generation and
Exponential Histogram construction remain separate work.

## Planner-layering follow-up: raw response latency

`CostModel::raw_query_response_latency_ms` quotes one execution separately from
amortized workload cost. A known quote that exceeds any bound of its consumers
cannot win against a feasible summary, and cannot reappear during final raw
fallback. If no costed summary survives, planning returns `NoLatencyFeasiblePlan`.
The public planner's `slow_cheap_raw_recompute_cannot_bypass_the_response_bound`
and `slow_raw_only_query_reports_no_latency_feasible_plan` reproduce both paths.
As with summary latency quotes, missing raw latency evidence remains unchecked;
this does not establish a latency guarantee for an unmeasured deployment.

## Planner-layering follow-up: typed frequency inputs

The native UnivMon build accepts Utf8, Int64 and Boolean frequency identities,
as well as Float64 samples. Typed keys prevent integer rounding and numeric
coercion from changing cardinality. NULL inputs are skipped as before; other
summary families keep their numeric input contracts. Variable-length heap keys
contribute to state memory accounting. `univmon_execution::typed_frequency_keys_preserve_identity`
executes one shared state with distinct/L2/entropy readers for each type,
including neighboring integers above 2^53. Kernel tests also merge and persist
string-key states and check memory after reset. SQL entropy/L2 idiom recognition
is not established by these native input tests.
