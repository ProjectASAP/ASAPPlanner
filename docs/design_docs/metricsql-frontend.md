# MetricsQL frontend boundary

Audience: planner and query-engine developers integrating VictoriaMetrics.

`asap-frontend-metricsql` vendors the `metricsql_parser` crate from
`ccollie/metricsql` commit
`3046709308e449a42c56bfbfd45f95af848e6768`. This Apache-2.0 Rust port is based
on VictoriaMetrics and models MetricsQL syntax directly, including `WITH`,
rollup expressions, step-relative durations, MetricsQL binary operators,
aggregate limits, or-delimited matchers, and `keep_metric_names`.

The frontend walks that AST directly and emits the existing canonical
`QueryExpr`. It does not add MetricsQL fields to `QueryExpr`, SDS descriptors,
or the physical summary DAG.

```text
MetricsQL source
      |
MetricsqlExpr (extension semantics retained)
      |
canonical QueryExpr
      |
existing ASAP-aware mapping and physical Summary DAG
```

## Extension behavior

| Syntax | Behavior |
|---|---|
| Exact-name selectors and ordinary label matchers | `Scan` with canonical predicates. |
| Fixed range selectors | `TimeRange`; step-relative ranges fail closed. |
| `sum`, `avg`, `min`, `max`, `count`, `stddev`, `stdvar`, `group`, `quantile` | Existing canonical aggregate intents, including `by` and `without`. |
| Common rollups: rate/increase/derivatives and statistical `*_over_time` | Existing per-entity canonical intents over the lowered range. |
| PromQL arithmetic, comparison, and set binary operators without modifiers | Existing canonical `BinaryOp`. |
| `default_rollup(selector[range])` | Lower to `Aggregate(LastOverTime)` over the explicit `TimeRange`. |
| `default_rollup(selector)` | Reject for exact fallback because the implicit lookbehind window depends on the runtime evaluation step, which is not a property of canonical `QueryExpr`. |
| `expr keep_metric_names` | Parsed natively, then rejected for exact fallback because canonical `QueryExpr` does not carry metric-name lineage. |
| `if`, `ifnot`, `default`, aggregate `limit`, or-delimited matchers, binary match modifiers | Parsed natively and rejected until the canonical executor has the exact semantics. |
| `WITH` | Expanded by the native parser; the expanded expression lowers when every resulting node is supported. |

Unsupported MetricsQL extensions return a typed error. The VictoriaMetrics
query boundary is responsible for routing that error to its exact backend.
Silently dropping `keep_metric_names` or inventing a fixed implicit rollup
window would change query results, so neither approximation is permitted.

`canonical_metricsql` supplies plan-catalog identity by rendering the parsed
native MetricsQL AST's `Display`. Formatting differences do not create distinct
identities, and unsupported extension semantics remain visible in the identity
rather than being erased.

The upstream parser's `metricsql_common` workspace crate requires nightly Rust
and AES CPU features for unrelated runtime utilities. ASAPPlanner vendors the
parser with a direct dependency on a stable, portable support crate containing
exactly the APIs it imports: duration formatting, hash collection aliases, and
datetime constant-evaluation helpers. The parser remains the pinned third-party
source with formatting-only changes. Vendoring both path crates
makes the stable parser dependency self-contained for downstream consumers;
Cargo does not propagate a workspace root `[patch]` into dependent projects.

Focused corpus tests use representative MetricsQL-only queries from
VictoriaMetrics' `app/vmselect/promql/exec_test.go` and generated MetricsQL
reference: `keep_metric_names`, `ifnot/default`, step-relative windows,
aggregate limits, or-delimited matchers, and `WITH` expansion. They assert that
the native parser accepts each form and that unsupported canonical lowering
fails closed.
