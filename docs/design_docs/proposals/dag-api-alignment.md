# DAG API alignment

Status: collection API unification implemented and validated on #480.
Audience: Planner developers and API integrators.

## Objective

Align the Rust API with the DAG names in
[Planner and deployment layering](planner-layering.md#dags-and-what-each-encodes),
without duplicating graph implementations or eagerly copying every candidate.

The public pipeline is:

```text
CandidatePreASAPDAGs
  -> CandidatePostASAPDAGs
  -> CandidateLifecyclePostASAPDAGs
  -> CandidatePhysicalDAGs
  -> selection with deployment-supplied prices
  -> deployment execution of the selected plan
```

Each generation stage retains supported legal alternatives. A workload entry
and an alternative for that entry are distinct identities. Timing comes from
lifecycle assignments; generation does not implicitly select a winner.

## Baseline

The implementation builds on the physical compilation and lifecycle APIs of the
stack under #508; main does not contain them yet.

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
   execution graphs on demand, including after selection. Preserve
   separate compilations when timing changes operator lowering. Keep failures
   attributable to their candidates.
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
- Equivalent timing cuts share one compilation; a realization is never reused
  for timing that lowers differently.
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
- Added `PostASAPDAGIndex`, retaining shared node references and projecting
  node and edge records once. Physical compilation takes a borrowed
  `PostASAPDAGView` of a transport document, an index, or a lifecycle
  assignment; an assignment's view overlays its timing on the index's records
  instead of copying them. Transport import and direct compilation share
  validation and operator lowering; no second rewrite implementation was introduced.
- Added the lazy, budget-checked timed collection `CandidateLifecyclePostASAPDAGs`.
  Unpriced legal choices retain unknown cost, and rejected combinations retain
  their choices and errors. `execution_assignment` attaches timing to the shared
  index. `SummaryMaintenanceLifecyclePlan` is merged into `LifecyclePostASAPDAG`:
  one DAG root with its per-state lifecycle, window framework and retention, from
  which the timing is derived; unresolved window evidence remains explicit and must be supplied before
  deployment installation.
- Added `compile_physical_dag_candidates` and `CandidatePhysicalDAGs`. Compatible
  assignments share `Arc<PhysicalDAG>`; timing that changes lowering produces a
  separate compilation. Cuts materialize on demand through the existing implementation.
- Kept explicit eager cut and winner-selection helpers for callers requesting
  them. These helpers are not the candidate-preserving generation pipeline.
- Added regression coverage for workload entry IDs, shared logical identity,
  assignment budgets and unknown costs, physical compilation sharing, agreement
  with independent compilation of each assignment's transport, and rejected timing.

The Rust API migration is documented in
[dag-api-migration.md](../../develop_docs/dag-api-migration.md). Downstream
consumers must adopt the breaking names when they update their Planner dependency.

## Collection boundary completion

The logical collection `CandidatePostASAPDAGs<Id>` and the timed collection
`CandidateLifecyclePostASAPDAGs<'a, Id>` are separate plain types. The timed
collection encapsulates the existing lifecycle enumerator: `with_timing_for_root`
handles logical realization, index sharing, assignment budgets and lazy
generation, and single already-assembled graphs use `from_post_asap_dag`. It
also answers `lifecycle_guarantee`, so a deployment can supply a price for an
alternative before selection binds it. A state without lifecycle alternatives yields a diagnostic
entry rather than disappearing.

`compile_physical_dag_candidates` consumes the timed collection's entries and
returns `CandidatePhysicalDAGs<M, E>`, which owns shared graphs and cut
descriptors. The metadata type `M` and the timing error type `E` belong to the
caller, so the physical crate does not depend on lifecycle planning; a timing
failure stays typed as `PhysicalCandidateError::Timing`, separate from
`PhysicalCandidateError::Compile`. Both transitions retain IDs, lifecycle
metadata and errors. Reuse also checks contracts and requested roots,
preventing one candidate's input boundary from contaminating another.

Existing explicit lifecycle selection and cut APIs delegate to the same
implementation. No second lifecycle enumeration or operator compiler was added.
Binding runtime sources is an execution step: `PhysicalDAG::instantiate`
returns a `PhysicalExecution` handle for one run, not another DAG.
