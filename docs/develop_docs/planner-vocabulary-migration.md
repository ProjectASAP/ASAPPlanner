# Planner vocabulary migration (#427)

> **Status: historical.** Most names in this table belonged to the legacy
> replacement search, which has been removed. The planner now plans through
> the stages described in [the architecture](asap-aware-mapping-architecture.md).
> The table is kept as a record of the rename.

This is a Rust source API rename. Update imports and call sites using the table
below. Planning, ranking, schema resolution, cost arithmetic, and window coverage
rules are unchanged. Old Rust names are removed rather than retained as a second
naming family; ordinary fluent `with_*` builders and SQL/PromQL syntax keep their
names.

| Previous name | Current name |
|---|---|
| `replacement::Implementation` | `replacement::Realization` |
| `replacement::ImplementError` | `replacement::RealizationError` |
| `ReplacementProvenance::SummaryImplementation` | `ReplacementProvenance::SummaryRealization` |
| `implementations_for_with` (internal) | `realizations_for_intent` |
| `pre_asap::binder` | `asap_frontend_common::schema_resolver` |
| `Binder` | removed; the free function below |
| `Binder::bind`, `Binder::bind_with_inherited` | `schema_resolver::resolve_schema_with_inherited` |
| `PanePhaseBinding` | `PaneLayout` |
| `BoundaryCoverage` | `WindowEdgeCoverage` |
| `ExactBoundaryResidual` | `ExactWindowEdgeResidual` |
| `WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactBoundaryResidual` | `WindowEdgeCompatibility::RequiresAlignedPanePhaseOrExactWindowEdgeResidual` |
| `resources::boundary` | `resources::physical_handoff` |
| `BoundaryKind` | `PhysicalHandoffKind` |
| `BoundaryResources` | `PhysicalHandoffBytes` |
| `asap_aware_mapping::boundary_cost` | removed |
| `PhysicalBoundary` | `PhysicalHandoff` |
| `BoundaryNodeEvidence`, `BoundaryPlanEvidence` | `PhysicalHandoffNodeEvidence`, `PhysicalHandoffPlanEvidence` |
| `BoundaryProfile`, `BoundaryCalibration`, `BoundaryEstimate` | `PhysicalHandoffProfile`, `PhysicalHandoffCalibration`, `PhysicalHandoffEstimate` |
| `BOUNDARY_MODEL_VERSION` | `PHYSICAL_HANDOFF_MODEL_VERSION` |
| `estimate_boundaries` | `estimate_physical_handoffs` |
| Physical evidence/comparison `boundaries` fields | `handoffs` |
| `BoundaryEstimate::per_boundary` | `PhysicalHandoffEstimate::per_handoff` |
| Internal `Models` | `CandidatePlanningInputs` |
| `SketchAlgorithmStrategy::with_models` | `ASAPStrategies::new_with_planning_inputs` |
| `SketchAlgorithmStrategy::with_models_and_evidence` | `ASAPStrategies::new_with_planning_inputs_and_evidence` |
| `SketchAlgorithmStrategy` | `ASAPStrategies` |
| `HydraGroupingStrategy::with_models_and_evidence` | `HydraGroupingStrategy::new_with_planning_inputs_and_evidence` |

For example, `Binder::new().bind(&dag)` becomes
`resolve_schema_with_inherited(&dag, &[])`. Cost-model implementations that
accept or return `Implementation` now use `Realization`; variants and ranking
contracts remain the same.

## Serialized compatibility

Existing JSON needs no migration. Serde retains the `boundaries` and
`per_boundary` keys, `exact_boundary_residual` tag, and
`RequiresAlignedPanePhaseOrExactBoundaryResidual` value. The planner-cost JSON
input still uses `boundaries`. Resource explanation labels (`boundary:<id>:<dimension>`)
and the `physical-boundary-bytes-v1` model version also remain unchanged. These
are compatibility labels, not the names to use in new Rust code.

See [the planner-runtime contract](../design_docs/architecture/planner-runtime-contract.md)
for ownership. Documentation now uses *planner-runtime contract* for
planner/downstream interaction.

The physical-handoff estimator (`physical_handoff_cost`, with
`PhysicalHandoffProfile` and the `resources::physical_handoff` types) and
`ComparisonScope` were later deleted: no planning stage read them.
