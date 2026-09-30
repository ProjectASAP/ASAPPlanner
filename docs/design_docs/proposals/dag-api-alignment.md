# DAG API alignment

Status: implementation plan. Audience: Planner developers and API integrators.

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
