# DAG API alignment

Status: naming and graph-sharing changes implemented and workspace-tested on
the #480 branch; the unified timed logical candidate collection API remains open.
Audience: Planner developers and API integrators.

## Objective

Align the Rust API with the DAG names in
[Planner and deployment layering](planner-backend-layering.md#dag-names),
without duplicating graph implementations or eagerly copying every candidate.

The public pipeline is:

```text
CandidatePreASAPDAGs
  -> CandidatePostASAPDAGs
  -> CandidatePostASAPDAGs with timing
  -> CandidatePhysicalDAGs
  -> deployment selection and execution
```

Each generation stage retains supported legal alternatives. A workload entry
and an alternative for that entry are distinct identities. Timing comes from
lifecycle assignments; generation does not implicitly select a winner.

## Baseline

The complete implementation depends on the physical compilation and lifecycle
integration underlying #508. Main and the original #480 documentation branch
do not yet contain those implementations. Apply the complete refactor on that
integration, preserving #480's documentation changes; do not merge main as part
of this task.

## Changes

1. **Name the existing representations.** Rename `QueryExpr` to `PreASAPNode`
   and `SummaryNode` to `PostASAPNode`. Define `PreASAPDAG` and `PostASAPDAG` as
   root-reference aliases, retaining shared subgraphs and the pre-ASAP column
   binding parameter. Rename `CompiledPhysicalDag` to `PhysicalDAG`. Update
   callers, exports, examples and tests together; avoid compatibility aliases
   that leave two public names for the same role.
2. **Name frontend candidate outputs.** Introduce `CandidatePreASAPDAGs` using
   the existing collection machinery. Preserve workload entry identity and
   distinguish alternative lowering results from independent workload roots.
   Deterministic lowering produces one alternative per entry. Do not add an
   independent frontend candidate search implementation.
3. **Use one authoritative logical graph.** Keep the shared `PostASAPNode` graph
   as `PostASAPDAG`. Move physical compilation onto this graph and reuse the
   existing node-identity mapping and validation. Keep a flat node/edge form
   only as an explicitly named transport document when serialization requires
   it; derive it from the authoritative graph. Do not maintain a second rewrite
   or computation implementation in the transport format.
4. **Attach lifecycle assignments without duplicating logical graphs.** Keep
   the existing compact `CandidatePostASAPDAGs` search representation. Reuse
   lifecycle enumeration and validation to expose candidate DAG references
   with timing, window and retention assignments. Enumerate combinations lazily
   or under an explicit expansion budget; budget exhaustion must not appear as
   a complete candidate collection. Do not create a second timed graph IR.
5. **Share physical compilation across timing cuts.** `CandidatePhysicalDAGs`
   owns shared `PhysicalDAG` realizations and lightweight candidate entries
   identifying their timing cuts, contracts and lifecycle metadata. Reuse the
   existing cut and validation implementation. Materialize precompute/query
   execution graphs on demand, including after deployment selection. Preserve
   separate compilations when timing changes operator lowering, notably an
   ingestion-time Binary. Keep failures attributable to their candidates.
6. **Update the public boundary and documentation.** Route collection APIs
   through the existing lowering, search, lifecycle and compilation algorithms.
   Preserve explicit opt-in selection helpers. Update the DAG naming table to
   describe implemented representations, and document breaking API migration.

## Acceptance and validation

- Public names correspond to individual nodes, DAG roots and candidate
  collections without ambiguous aliases or redundant computation algorithms.
- Frontend entry identity survives lowering; independent queries are not
  presented as mutually exclusive alternatives.
- Shared logical subgraphs retain identity through enumeration and compilation.
- Lifecycle alternatives reuse the logical graph and assign all required node
  timing, window and retention information. Illegal timing edges are rejected.
- Equivalent timing cuts share one compilation. Timing-dependent Binary
  lowering remains correct and cannot reuse an incompatible realization.
- Candidate generation preserves valid alternatives and visible rejection
  reasons; candidate inspection never silently invokes winner selection.
- Selected physical execution graphs preserve their typed producer/reader
  contracts, results and serialization validation.
- Add focused regression tests for graph identity, timing and candidate sharing;
  run formatting, workspace clippy and the workspace test suite. Review docs,
  migration examples and any affected downstream call sites.

Implement in reviewable commits: naming and frontend collection; logical graph
and timing consolidation; physical candidate sharing; documentation and final
integration validation. Record actual completion and any remaining limitations
rather than treating a rename as completion of the structural work.

## Implementation record

- Named the existing node types and shared root aliases; added frontend
  `lower_pre_asap_dag_candidates` and the ID-preserving `CandidatePreASAPDAGs` collection.
- Added `PostASAPDAGIndex`, retaining shared node references and edge metadata.
  Physical compilation accepts the root, index or timing assignment through a
  borrowed projection. Transport import and direct compilation share validation
  and operator lowering; no second rewrite implementation was introduced.
- Added lazy, budget-checked lifecycle `assignments`. Unpriced legal choices
  retain unknown cost, and rejected combinations retain their choices and errors.
  `execution_assignment` attaches timing to the shared index. Window and
  retention metadata stay on the accompanying lifecycle plan; unresolved window
  evidence remains explicit and must be supplied before deployment installation.
- Added `compile_timed_candidates` and `CandidatePhysicalDAGs`. Compatible
  assignments share `Arc<PhysicalDAG>`; Binary timing changes produce a separate
  compilation. Cuts materialize on demand through the existing implementation.
- Kept explicit eager cut and winner-selection helpers for callers requesting
  them. These helpers are not the candidate-preserving generation pipeline.
- Added regression coverage for workload entry IDs, shared logical identity,
  assignment budgets and unknown costs, physical compilation sharing,
  timing-dependent Binary separation, transport equivalence and rejected timing.

The Rust API migration is documented in
[dag-api-migration.md](../../develop_docs/dag-api-migration.md). Downstream
consumers must adopt the breaking names before repinning to this branch.

Validation: `cargo test --workspace` passed all 1507 tests across 91 test groups.
Formatting, strict all-target clippy, Markdown links and whitespace are checked
before publishing this implementation.

## Remaining API alignment

The lifecycle enumerator is still public as
`SummaryMaintenanceLifecycleCandidates`, and callers compose its assignments
with logical candidates themselves. This does not yet implement a unified
`CandidatePostASAPDAGs` with timing collection boundary. Encapsulate the existing
enumerator behind that boundary, preserving graph sharing, lifecycle metadata,
rejections and lazy generation; do not rename it as though it already contains
the logical candidate space or duplicate its enumeration algorithm.
