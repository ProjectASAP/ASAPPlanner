# Target candidate and DAG assembly API migration (#456)

This source-only rename follows the input/output/workflow design in #445.
Candidate generation, ordering, accuracy checks, selection, and runtime behavior
are unchanged. #453 separately defines the integration API surface.

| Previous name | New name | Meaning |
|---|---|---|
| `CandidateLogicalASAPDAGs::groups()` | `CandidateLogicalASAPDAGs::target_subdag_candidates()` | Iterate candidate sets, one per target, in discovery order |
| `CandidateLogicalASAPDAGs::group_for(target)` | `CandidateLogicalASAPDAGs::candidates_for_target(target)` | Look up one target's candidate set |
| `SelectedGroup` | `TargetSubDAGSelection` | Selected choice and usage information for one target; the choice may be absent |
| `GlobalSelection::groups()` | `GlobalSelection::target_selections()` | Iterate decisions, not alternative sets |
| `MaterializeSummaryMaintenanceLifecycleError` | `SummaryMaintenanceLifecycleAssemblyError` | Failure assembling a DAG or deriving maintenance decisions |
| Error variant `Materialize` | `AssembleDAG` | Wrap an underlying `RealizationError` from DAG assembly |
| Internal `materialize_inner` / `materialize_residual` | `assemble_target` / `assemble_residual` | Assemble selected nodes, not runtime materialized views |
| Internal assembly cache `materialized` | `assembled_nodes` | Preserve shared node identity (now `Rc<OperatorNode>`, see below) |

Update imports and calls together; old public names are not retained as aliases.
Downstream Rust integrations using these symbols must migrate. No serialized
plan schema changes. SQL grouping and physical materialization keep their names.

The earlier #445 renames (`TargetSubDAGCandidates`,
`RankedTargetSubDAGCandidates`, `assemble_selected_dag`, and its lifecycle-aware
counterpart) are prerequisites, not additional changes here.

The workflow remains one selection call per workload followed by one assembly
call per query root. The summary-maintenance lifecycle API named above was later
removed; Stage 2 materialization (#509) will own maintenance decisions.

## Later: unified operator IR (operator flattening)

The pre-ASAP and post-ASAP trees became one IR in `asap_types::ir`. Every
node is an `Rc<OperatorNode>` whose `operator` is `Operator::NonASAP(NonASAPOp)`
or `Operator::ASAP(ASAPOp)`. Old public names are not kept as aliases.

| Old | New |
|---|---|
| `Rc<QueryExpr>` (pre-ASAP) | `Rc<OperatorNode>` holding `Operator::NonASAP(NonASAPOp)` |
| `Rc<SummaryNode>` / `SummaryExpr` (post-ASAP) | The same `Rc<OperatorNode>`; summary steps are `Operator::ASAP(ASAPOp)` |
| `SummaryExpr::KeepPreAsap(q)` | The non-ASAP sub-DAG itself; `retain_exact` only adds an exact `guarantee` |
| `SummaryExpr::ValueOperation { .. }` over a evaluation | An ordinary `NonASAPOp` (`Project`, `Filter`, `Sort`, `Limit`, `Aggregate`) reading an ASAP node; `FinalizeExactAccumulator`, `MaintainPopulation`, `EvaluatePopulation` are `ASAPOp` variants |
| `Replacement::Summary(..)` / `Replacement::Rewrite(..)` | `Replacement::SubDAG(Rc<OperatorNode>)`; `is_logical_rewrite` tells them apart |
| `SummaryFamilyType` | `FieldDataType` (its non-`Plain` variants) |
| Timing stored on post-ASAP nodes | `OperatorNode::timing`, `None` until `ir::timing::apply_materialization_timings` writes it from a `MaterializationAssignment` (default: all query time) |
| `UnresolvedQueryExpr` + `asap_types::pre_asap::resolve_root` | `UnresolvedOp` / `UnresolvedScalar` + `asap_frontend_common::resolve_root` |
| `pre_asap::canonicalize`, `pre_asap::cse::share_common_sub_dags` | `ir::canonicalize::canonicalize`, `ir::cse::share_common_sub_dags` |
| `asap_types::post_asap::compile_post_asap_dag` (wire version 5, `Fallback`/`Binary`/`Value` payloads) | `asap_types::ir::export::compile_post_asap_dag` (wire version 7: one node per operator, `Relational` payloads, `ScalarRef` edges); input must be timed |
| Exported schema JSON `columns` | `fields` |

Field and schema details: [Pre-ASAP IR](pre-asap-ir.md) and
[Post-ASAP IR](../design_docs/concepts/post-asap-ir.md).
