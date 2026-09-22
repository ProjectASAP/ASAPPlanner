# Target candidate and DAG assembly API migration (#456)

This source-only rename follows the input/output/workflow design in #445.
Candidate generation, ordering, accuracy checks, selection, and runtime behavior
are unchanged. #453 separately defines the integration API surface.

| Previous name | New name | Meaning |
|---|---|---|
| `PlanSpace::groups()` | `PlanSpace::target_subdag_candidates()` | Iterate candidate sets, one per target, in discovery order |
| `PlanSpace::group_for(target)` | `PlanSpace::candidates_for_target(target)` | Look up one target's candidate set |
| `SelectedGroup` | `TargetSubDAGSelection` | Selected choice and usage information for one target; the choice may be absent |
| `GlobalSelection::groups()` | `GlobalSelection::target_selections()` | Iterate decisions, not alternative sets |
| `MaterializeSummaryMaintenanceLifecycleError` | `SummaryMaintenanceLifecycleAssemblyError` | Failure assembling a DAG or deriving maintenance decisions |
| Error variant `Materialize` | `AssembleDag` | Wrap an underlying `RealizationError` from DAG assembly |
| Internal `materialize_inner` / `materialize_residual` | `assemble_target` / `assemble_residual` | Assemble selected nodes, not runtime materialized views |
| Internal assembly cache `materialized` | `assembled_nodes` | Preserve shared `Rc<SummaryNode>` identity |

Update imports and calls together; old public names are not retained as aliases.
Downstream Rust integrations using these symbols must migrate. No serialized
plan schema changes. SQL grouping and physical materialization keep their names.

The earlier #445 renames (`TargetSubDAGCandidates`,
`RankedTargetSubDAGCandidates`, `assemble_selected_dag`, and its lifecycle-aware
counterpart) are prerequisites, not additional changes here.

The workflow remains one selection call per workload followed by one assembly
call per query root. `SummaryMaintenanceLifecyclePlan` contains the assembled
Post-ASAP DAG root plus maintenance decisions; it is not an executable plan.
