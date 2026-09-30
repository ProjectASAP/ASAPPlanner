# DAG API migration

Audience: Rust integrators. This is a breaking API change on the #508 integration.
There are no compatibility aliases for the former graph names.

| Previous API | Replacement |
|---|---|
| `QueryExpr`, `ResolvedQueryExpr`, `UnresolvedQueryExpr`, `QueryExprError` | `PreASAPNode`, `ResolvedPreASAPNode`, `UnresolvedPreASAPNode`, `PreASAPNodeError` |
| `Rc<QueryExpr>` | `PreASAPDAG` (same shared root representation) |
| `SummaryNode`, `Rc<SummaryNode>` | `PostASAPNode`, `PostASAPDAG` |
| `PlanSpace<Id>` | `CandidatePostASAPDAGs<Id>` |
| `PostAsapDag`, `PostAsapDagDocument` | `PostASAPDAGTransport`, `PostASAPDAGDocument` for explicit transport |
| `compile_post_asap_dag`, `compile_post_asap_dag_with_node_ids` | `export_post_asap_dag`, `export_post_asap_dag_with_node_ids` |
| `execution_timed_dag` | `export_timed_dag` for transport; use `execution_assignment` for shared compilation |
| `CompiledPhysicalDag` | `PhysicalDAG` |
| Runtime-bound `PhysicalDag` | `PhysicalExecution` |

## Candidate generation

`asap_planner::lower_pre_asap_dag_candidates(&input).await` returns
`CandidatePreASAPDAGs<usize>`, keyed by normalized workload entry index. Batch
and repeating entries retain their identities. Current frontends lower each
entry deterministically. This function does not invoke an optimization pass.
`search_workload` and the target-aware search APIs consume the same root/ID
collection and produce the compact `CandidatePostASAPDAGs<Id>`.

The normal stage transition is:

```rust,ignore
let timed = logical.with_timing_for_root(
    &workload_entry_id,
    timing_context,
    logical_expansion_limit,
    assignment_expansion_limit,
)?;
let physical = compile_physical_dag_candidates(&timed, |metadata, assignment| {
    // Supply typed contracts and requested root IDs for this realization.
    resolve_contracts(metadata, assignment)
});
```

`timed` has type `CandidatePostASAPDAGs<Id, WithTiming<'a, Id>>`; `physical`
has type `CandidatePhysicalDAGs<Id>`. `CandidateTimingContext` binds the root's
`WorkloadDemand`, planning clock, horizon, lifecycle capabilities and cost model.
No winner-selection helper runs during these transitions.

The timed collection enumerates assignments lazily and owns the shared graph
indices. Both expansion budgets are checked before iteration, including the
total assignment count across logical alternatives. Rejected logical assemblies
remain accessible through `rejected_assemblies()`. Iterator entries retain
workload ID, logical/assignment indices, choices, lifecycle plan and timing or
rejection. Unknown cost stays unknown; absent window evidence must still be
resolved before installation.

Callers with an already assembled logical graph enter the same collection via
`CandidatePostASAPDAGs::from_post_asap_dag(id, root, context, limit)`.
`lifecycle_alternatives(logical_index)` supports inspection;
`select_lifecycles(logical_index, choices)` remains an explicit opt-in selection
operation. The former `enumerate_summary_maintenance_lifecycles` function and
`SummaryMaintenanceLifecycleCandidates` type are now internal implementation
machinery, not public layer outputs.

`compile_physical_dag_candidates` accepts the timed collection directly; it
replaces the tuple-based `compile_timed_candidates` API. Its contract resolver
can supply different inputs and roots for different realizations. The physical
collection keeps every timing or compilation failure with its original identity.
It exposes shared `PhysicalDAG`s through `iter()`, frontiers through
`frontier(index)`, and execution cuts through `materialize(index)`.
The former public `PhysicalDAGCandidate` wrapper is removed. Cut descriptors and
compilation reuse are internal to `CandidatePhysicalDAGs`, which is now a struct
rather than an alias for manually assembled tuples.

Whole-workload selection must still account for shared state and compatible
assignments across roots. Independent per-root minima do not prove a workload
minimum. Existing explicit selection helpers remain available.

## Representation and validation

`PostASAPDAG` is the authoritative shared logical graph. Its index stores node
references and edge metadata; it does not own another set of operator payloads.
The compiler uses transient node projections and the same validator and
operator compiler used for imported transport documents. Export a flat graph
only when a transport consumer needs it. Serialization contracts remain checked.

Timing is a total assignment over indexed node IDs. Missing assignments and
query-time producers feeding ingestion-time consumers are rejected. Assignments
share the index and graph; lifecycle choices do not clone logical operators.

Physical candidate generation shares each compiled graph across assignments
with the same index, Binary timings, input contracts and requested roots. Ingestion-time Binary changes lowering,
so incompatible assignments get separate compilations. Candidate cuts are
materialized on demand using the existing cut implementation. The convenience
`compile_candidate` and `compile_candidates` APIs still eagerly materialize
explicit requested cuts; they are not the shared candidate-generation path.
