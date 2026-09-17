# Post-ASAP IR reference

Audience: developers constructing, validating or consuming Post-ASAP plans.
Read the [conceptual introduction](../design_docs/concepts/post-asap-ir.md) first.
The authoritative node definitions are in
[expr.rs](../../crates/types/src/post_asap/expr.rs); use them for complete fields
and signatures rather than treating this catalog as a copied Rust enum.

| `SummaryExpr` variant | Contract |
| --- | --- |
| `KeepPreAsap` | Retain an exact `Rc<QueryExpr>` subtree with a plain output schema |
| `SummaryAgg` | Produce the selected family state with committed parameters, update input, reduction and grouping layout |
| `SummaryEstimate` | Read a statistic from compatible summary state |
| `SummaryMerge` | Merge compatible state inputs |
| `SummaryJoin` | Combine summary states for join estimation; distinct from a relational row join |
| `SummarySubtract` | Subtract states only when the representation supports it |
| `SummaryDelete` | Apply a supported deletion to state |
| `BinaryOp` | Compose independently planned operands while retaining binary and timing semantics |
| `ValueOperation` | Apply aggregate, exact, population, projection, filter, sort, limit or extension semantics with explicit execution timing |
| `RelationalJoin` | Join row-producing children while preserving kind and predicate |
| `CandidateTopK` | Propose candidate members and rank by authoritative values under a completeness contract |

## Construction and validation

Keep schema, family/parameter identity, grouping, input semantics and guarantees
consistent with the expression. Summary-state fields and plain values are not
interchangeable. An exact outer operation over approximate values still needs a
composed result guarantee.

[Execution-data-state validation](../../crates/types/src/post_asap/execution_data_state.rs)
checks maintenance/read-time consumption and production. A node's presence in
this enum does not imply that every backend or physical estimator implements it.
Preserve typed guards and fallbacks on conditional binary operations.

## Export and handoff

Use the [library export guide](library-api.md#export-and-explain) to distinguish
inspection graphs, semantic executable DAGs, and lifecycle-aware exports.
Constructing a serializable envelope does not validate it, and compiling a
semantic DAG does not allocate or deploy physical resources.

See [physical operator evidence](physical-operator-reference.md) for physical
binding and costing, and the
[accuracy companion](../design_docs/proposals/asap-aware-mapping/end-to-end-accuracy-guarantees-developer-guide.md)
for guarantee derivation and rejection behavior.
