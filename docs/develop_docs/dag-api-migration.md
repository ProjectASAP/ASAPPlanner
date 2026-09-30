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
| `PostAsapNodeId`, `PostAsapOperatorPayload`, `PostAsapDagNode`, `PostAsapDagEdge`, `PostAsapDagValidationError`, `PostAsapNodeIdentityMap`, `PostAsapSubstitution` | `PostASAPNodeId`, `PostASAPOperatorPayload`, `PostASAPDAGNode`, `PostASAPDAGEdge`, `PostASAPDAGValidationError`, `PostASAPNodeIdentityMap`, `PostASAPSubstitution` |
| `InvalidPostAsapDag` error variants | `InvalidPostASAPDAG` |
| `compile_post_asap_dag` | `export_post_asap_dag` |
| `compile_post_asap_dag_with_node_ids`, `PostAsapDagCompilation` | `index_post_asap_dag` returning `PostASAPDAGIndex` (`node_ids`, `view()`, `to_transport()`) |
| `execution_timed_dag` | `export_timed_dag` for transport; use `execution_assignment` for shared compilation |
| `CompiledPhysicalDag` | `PhysicalDAG` |
| Runtime-bound `PhysicalDag` | `PhysicalExecution`, the execution handle returned by `PhysicalDAG::instantiate` |
| `compile(&dag, …)`, `frontier_from_timing(&dag)` over a transport | `compile(dag.as_view(), …)`, `frontier_from_timing(dag.as_view())`; an index or assignment passes `view()` |
| `enumerate_summary_maintenance_lifecycles`, `SummaryMaintenanceLifecycleCandidates` | `CandidatePostASAPDAGsWithTiming` (see below) |

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
let physical = compile_physical_dag_candidates(
    timed.iter(),
    timed.rejected_assemblies().to_vec(),
    |metadata, assignment| {
        // Supply typed contracts and requested root IDs for this realization.
        resolve_contracts(metadata, assignment)
    },
);
```

`timed` has type `CandidatePostASAPDAGsWithTiming<'a, Id>`; `physical` has type
`CandidatePhysicalDAGs<PostASAPCandidateMetadata<Id>, Rc<CandidateTimingError>>`.
`CandidateTimingContext` binds the root's `WorkloadDemand`, planning clock,
horizon, lifecycle capabilities and cost model. No winner-selection helper runs
during these transitions.

The timed collection enumerates assignments lazily and owns the shared graph
indices. Both expansion budgets are checked before iteration, including the
total assignment count across logical alternatives; exceeding either returns
`CandidateTimingError::ExpansionLimit`. Rejected logical assemblies remain
accessible through `rejected_assemblies()`. Iterator entries retain workload ID,
logical/assignment indices, choices, lifecycle plan and timing or rejection. A
state with no lifecycle alternative appears as one entry with a `NoAlternatives`
error. Unknown cost stays unknown; absent window evidence must still be resolved
before installation.

Callers with an already assembled logical graph enter the same collection via
`CandidatePostASAPDAGsWithTiming::from_post_asap_dag(id, root, context, limit)`.
`lifecycle_alternatives(logical_index)` supports inspection,
`lifecycle_guarantee(logical_index, lifecycle)` prices an alternative before it
is bound, and `select_lifecycles(logical_index, choices)` remains an explicit
opt-in selection operation.

`compile_physical_dag_candidates` accepts any iterator of
`(metadata, Result<PostASAPDAGAssignment, E>)`, so the physical crate does not
depend on the mapping crate. Its contract resolver can supply different inputs
and roots for different realizations. The physical collection keeps every
timing or compilation failure with its original metadata:
`PhysicalCandidateError::Timing(E)` keeps the caller's typed timing error, and
`PhysicalCandidateError::Compile` holds a compilation error. It exposes shared
`PhysicalDAG`s through `iter()`, frontiers through `frontier(index)`, and
execution cuts through `materialize(index)`. Cut descriptors and compilation
reuse are internal to `CandidatePhysicalDAGs`.

Whole-workload selection must still account for shared state and compatible
assignments across roots. Independent per-root minima do not prove a workload
minimum. Existing explicit selection helpers remain available.

## Representation and validation

`PostASAPDAG` is the authoritative shared logical graph. Its index keeps the
shared node references and projects node and edge records once for
compilation; the compiler uses the same validator and operator compiler for
those records and for imported transport documents. Export a flat graph
only when a transport consumer needs it. Serialization contracts remain checked.

Timing is a total assignment over indexed node IDs. Missing assignments and
query-time producers feeding ingestion-time consumers are rejected. Assignments
share the index and graph; an assignment's `view()` overlays its timing on the
index's records, so read node and edge states through the view's `timing`,
`output_state` and `edge_state`. Lifecycle choices do not clone logical operators.

Physical candidate generation shares each compiled graph across assignments
with the same index, Binary timings, input contracts and requested roots.
Ingestion-time Binary changes lowering, so incompatible assignments get
separate compilations. Candidate cuts are
materialized on demand using the existing cut implementation. The convenience
`compile_candidate` and `compile_candidates` APIs still eagerly materialize
explicit requested cuts; they are not the shared candidate-generation path.
