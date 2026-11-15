//! Candidate scope: each logical candidate's physical alternatives, one per
//! valid $p^{\mathrm{phys}} = (\tau, \ell)$.

use crate::ir::properties::ExecutionTiming;
use crate::stages::CandidateForMemoGroupId;
use crate::stages::{Lifetime, PhysicalProperties};

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

/// $\alpha^{\mathrm{phys}}_{u,c,i}$: timing and lifetime only (the concepts
/// doc's table, section 2). Alignment is checked per context in Stage 3 with
/// [`PhysicalASAPMemoCandidates::delivers_on`](crate::stages::PhysicalASAPMemoCandidates::delivers_on).
pub fn producer_satisfies_physical_requirements(
    producer: &PhysicalCandidate,
    consumer_timing: ExecutionTiming,
) -> bool {
    use ExecutionTiming::{IngestionTime, QueryTime};
    match (
        producer.physical_properties.timing(),
        producer.physical_properties.lifetime(),
    ) {
        (QueryTime, _) => consumer_timing == QueryTime,
        (IngestionTime, Lifetime::None) => consumer_timing == IngestionTime,
        // Materialized at ingestion: read at ingestion time and at query time.
        (IngestionTime, Lifetime::AcrossEvaluations(_)) => true,
        (IngestionTime, Lifetime::OneEvaluation(_)) => {
            unreachable!("rejected by `PhysicalProperties::new`")
        }
    }
}
