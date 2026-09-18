# Planning enum audit and migration

This records the current code audit for issue #428 for library and wire-contract
consumers. Retention decisions depend on implemented consumers, not future uses.

| Type or variant | Decision | Current consumer or redundancy |
|---|---|---|
| `BoundaryKind` | Keep | `boundary_cost::estimate_boundaries` distinguishes network endpoint validation and network bytes from materialization bytes. |
| `CacheProfile` | Keep | `analytical_cost` resolves cache evidence into hit/miss work; `physical_plan_cost_model` rejects cache evidence for boundary/storage models that cannot account for it. |
| `SummaryWindowFramework::ExponentialHistogram` | Keep | `summary_maintenance_cost/window.rs` checks framework-specific accuracy evidence; candidate ranking composes its error with the root guarantee and checks the required accuracy. |
| `SummaryWindowFramework::Extension(String)` | Remove | Only the name was validated; no registered implementation or window-specific semantic validation consumed it. |
| `EvaluationSchedule` | Keep | Lifecycle planning emits one-shot, per-update, or on-read evaluation. The estimator rejects guarantees whose schedule disagrees with lifecycle and data arrival. Prepared state can be one-shot or per-update, depending on arrival. |
| `QueryTimeScope` | Keep | Lifecycle planning requires deletion support for moving real-time/mixed lookbacks, but not historical as-of reads. |
| `Predictability` | Keep | A predictable one-time query with advance notice can use prepared state; its known-at timestamp bounds activation. |
| `ExecutableOperator` and `ExecutableDagNode.operator` | Remove | Every tag duplicated the tagged payload and allowed inconsistent node states. Match the payload in Rust and read `payload.kind` in JSON. |
| Planner-local `CostUnit` | Consolidate | Mapping re-exports `asap_types::cost::CostUnit`. Recurring formulas require `CostUnitsPerSecond`; totals are rejected. |
| `MaterializationMedium` | Remove | No in-repository estimator or export consumer branches on memory, disk, or object store. All use the same materialization byte counter and coefficient. |

## Consumer migration

The executable DAG document version is **2**. Nodes no longer contain an
`operator` field: for example, a summary aggregate has only
`"payload": {"kind": "summary_agg", ...}` for its operator identity. Update
consumers to match `ExecutableOperatorPayload` or inspect `payload.kind`.
Version 1 documents are not supported; regenerate them with the new compiler.
The strict node decoder rejects the removed field.

Boundary materialization JSON changes from
`{"kind":"materialization","medium":"disk"}` to
`{"kind":"materialization"}`. Rust callers use `BoundaryKind::Materialization`.
Remove the medium from supplied boundary profiles. Network evidence and byte
counters are unchanged. A boundary is still explicitly declared; ordinary
in-memory dataflow does not become a materialization.

Window frameworks accept `tumbling`, `sliding`, and `exponential_histogram`.
Opaque `{"extension":"..."}` values are rejected. New frameworks need defined
planning and accuracy semantics before entering this contract.

Existing mapping imports of `CostUnit` remain valid as re-exports of the shared
type. Its existing shared serde representation is unchanged; `as_str()` retains
`cost_units_per_second` and also names totals as `cost_units`.

## Known downstream migration

The local ASAPQuery backend checkout still imports `ExecutableOperator` in
`crates/asap_types/src/executable_plan.rs` and constructs the duplicated field
in runtime tests. Those consumers must migrate to the payload before upgrading
the planner dependency. Its window compiler uses `Extension` only in a test
that rejects an unsupported hierarchical rollup; move that check to invalid
wire input or an unsupported layout. No materialization-medium consumer was
found there. This PR changes ASAPPlanner only.
