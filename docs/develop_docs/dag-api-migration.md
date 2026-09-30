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
| Runtime-bound `PhysicalDag` | `BoundPhysicalDAG` |

## Candidate generation

`asap_planner::lower_candidates(&input).await` returns
`CandidatePreASAPDAGs<usize>`, keyed by normalized workload entry index. Batch
and repeating entries retain their identities. Current frontends lower each
entry deterministically. This function does not invoke an optimization pass.
`search_workload` and the target-aware search APIs consume the same root/ID
collection and produce the compact `CandidatePostASAPDAGs<Id>`.

Use `enumerate_candidate_dags` or `enumerate_candidate_dags_for_root` with an
explicit expansion limit to obtain logical realizations. Rejected assemblies
remain visible. This is not a call to `global_selection`.

For each logical root:

1. Build one shared `Rc<PostASAPDAGIndex>` with `index_post_asap_dag(&root)`.
2. Call `enumerate_summary_maintenance_lifecycles` with its workload demand,
   capabilities and evidence. Call `assignments(limit)` to enumerate combinations
   lazily. Each item contains its choices and a plan or rejection. The limit is
   checked before yielding, so exhaustion cannot silently truncate candidates.
3. For a successful plan, call `plan.execution_assignment(index.clone())`.
   Keep the plan alongside the assignment: it owns the lifecycle, window,
   retention and cost metadata. An absent cost remains unknown. An absent
   window framework is unresolved deployment evidence, not an implicit default.
4. Pass `(metadata, assignment)` pairs to `compile_timed_candidates`, with the
   input contracts and requested root IDs. The returned
   `CandidatePhysicalDAGs<Metadata>` keeps metadata even when compilation fails.
   Use the same index for assignments of one logical root. Input contracts and
   requested roots are fixed per call; different contracts need a separate call.
5. Inspect a candidate's shared compiled graph and frontier for pricing, or
   call `materialize()` when its precompute/query cuts are needed. Errors from
   cut materialization remain errors; do not silently substitute another plan.
   After selection, bind sources and execute the selected cuts.

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
with the same index and Binary timings. Ingestion-time Binary changes lowering,
so incompatible assignments get separate compilations. Candidate cuts are
materialized on demand using the existing cut implementation. The convenience
`compile_candidate` and `compile_candidates` APIs still eagerly materialize
explicit requested cuts; they are not the shared candidate-generation path.
