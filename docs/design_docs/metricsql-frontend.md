# MetricsQL frontend boundary

Audience: planner and query-engine developers integrating VictoriaMetrics.

`asap-frontend-metricsql` parses MetricsQL into a language-owned AST and lowers
supported expressions into the existing canonical `QueryExpr`. It does not add
MetricsQL fields to `QueryExpr`, SDS descriptors, or the physical summary DAG.
PromQL-compatible AST nodes use the same AST-to-canonical lowering so selector,
range, aggregation, function, and accuracy behavior stays aligned.

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
| PromQL-compatible selectors, ranges, aggregations, calls, and binary expressions | Lower to the same canonical shapes as the PromQL frontend. |
| `default_rollup(selector[range])` | Lower to `Aggregate(LastOverTime)` over the explicit `TimeRange`. |
| `default_rollup(selector)` | Reject for exact fallback because the implicit lookbehind window depends on the runtime evaluation step, which is not a property of canonical `QueryExpr`. |
| `expr keep_metric_names` | Preserve as a MetricsQL AST node, then reject for exact fallback because canonical `QueryExpr` does not carry metric-name lineage. |

Unsupported MetricsQL extensions return a typed error. The VictoriaMetrics
query boundary is responsible for routing that error to its exact backend.
Silently dropping `keep_metric_names` or inventing a fixed implicit rollup
window would change query results, so neither approximation is permitted.

`canonical_metricsql` supplies plan-catalog identity by rendering the parsed
`MetricsqlExpr`. Compatible syntax uses the parser AST display, while extension
nodes recursively render their canonical child. Formatting differences do not
create distinct identities, and unsupported extension semantics remain visible
in the identity rather than being erased.

The parser currently recognizes MetricsQL-only nodes at the root. Nested
MetricsQL-only calls and modifiers remain exact-fallback cases until the native
AST grammar covers them.
