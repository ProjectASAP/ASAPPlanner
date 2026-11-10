//! Candidate scope: each logical candidate's physical alternatives, one per
//! valid $p^{\mathrm{phys}} = (\tau, \ell)$ (#1200).

use crate::ir::properties::ExecutionTiming;
use crate::stages::PhysicalProperties;
use crate::stages::{CandidateForMemoGroupId, ExecutionContext, PhysicalASAPMemoCandidates};

/// Index into [`PhysicalASAPMemoCandidates::physical_candidates`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct PhysicalCandidateId(pub usize);

/// One physical alternative of a logical candidate: what Stage 3 selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalCandidate {
    /// $e_v$ and $p^{\log}_v$.
    pub logical_candidate_id: CandidateForMemoGroupId,
    /// $p^{\mathrm{phys}}_v$.
    pub physical_properties: PhysicalProperties,
}

impl PhysicalCandidate {
    /// Whether a consumer running in `context` can read this producer (the
    /// concepts doc's alignment table, section 2). True for $\ell$ = None,
    /// and for `OneEvaluation` in the same context. A chain producer
    /// (`SummaryTumblingWindowChain`) with $\ell$ = `AcrossEvaluations`, with
    /// $w$ the chain operator's width, needs every time of the context on
    /// its grid: $w$ divides the context's period and phase. Any other
    /// producer with $\ell$ = `AcrossEvaluations` is stored on its group's
    /// evaluation grid (`evaluation_grid_of_group`): it delivers on every
    /// context when the group does not depend on the evaluation time, else
    /// when that grid holds every time of the context. In
    /// `Materialized(v)`, the context's times are v's, read from
    /// `candidates`.
    pub fn delivers_on(
        &self,
        candidates: &PhysicalASAPMemoCandidates,
        context: &ExecutionContext,
    ) -> bool {
        let _ = (candidates, context);
        todo!("#1207 PR 1: the alignment table of section 2")
    }
}

/// $\alpha^{\mathrm{phys}}_{u,c,i}$: timing and lifetime only (the concepts
/// doc's table, section 2). Alignment is checked per context in Stage 3 with
/// [`PhysicalCandidate::delivers_on`].
pub fn producer_satisfies_physical_requirements(
    producer: &PhysicalCandidate,
    consumer_timing: ExecutionTiming,
) -> bool {
    let _ = (producer, consumer_timing);
    todo!("#1207 PR 1: the timing x lifetime table of section 2")
}
