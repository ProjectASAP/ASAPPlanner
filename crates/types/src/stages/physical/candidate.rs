//! Candidate scope: each logical candidate's physical alternatives, one per
//! valid $p^{\mathrm{phys}} = (\tau, \ell)$.

use crate::ir::properties::ExecutionTiming;
use crate::ir::{ASAPOp, Operator};
use crate::stages::{CandidateForMemoGroupId, ExecutionContext, PhysicalASAPMemoCandidates};
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

impl PhysicalCandidate {
    /// Whether a consumer running in `context` can read this producer (the
    /// concepts doc's alignment table, section 2): true for $\ell$ = None,
    /// which runs in the consumer's own context; for `OneEvaluation` only in
    /// an `Evaluation` context, the one evaluation its result belongs to.
    /// With $\ell$ = `AcrossEvaluations`, every execution time of the
    /// context must be a time the producer has stored: for a chain
    /// (`SummaryTumblingWindowChain`) of width $w$, $w$ divides an
    /// evaluation grid's period and phase, or a refresh window's width and
    /// phase; any other result is stored on its group's evaluation grid
    /// (`evaluation_grid_of_group`), which must hold every time of the
    /// context, unless the group does not depend on the evaluation time
    /// (then it delivers on every context). The times of a
    /// `Materialized(v)` context are $v$'s own: every window end of a chain
    /// $v$ (its width at phase 0), else its group's evaluation grid; with
    /// neither they are not known, and no time-dependent producer delivers
    /// on it.
    pub fn delivers_on(
        &self,
        candidates: &PhysicalASAPMemoCandidates,
        context: &ExecutionContext,
    ) -> bool {
        match self.physical_properties.lifetime() {
            Lifetime::None => true,
            Lifetime::OneEvaluation(_) => matches!(context, ExecutionContext::Evaluation { .. }),
            Lifetime::AcrossEvaluations(_) => {
                let logical = &candidates.logical_candidates;
                let group = self.logical_candidate_id.group;
                let chain_tumbling_window_width_ms =
                    self.chain_tumbling_window_width_ms(candidates);
                let depends_on_evaluation_time = logical.memo.groups()[group.0 as usize]
                    .group_properties
                    .depends_on_evaluation_time;
                if chain_tumbling_window_width_ms.is_none() && !depends_on_evaluation_time {
                    return true;
                }
                let (period_ms, phase_ms) = match context {
                    ExecutionContext::Evaluation { grid } => (grid.period_ms, grid.phase_ms),
                    ExecutionContext::IngestionPipeline(window) => {
                        (window.width_ms, window.phase_ms)
                    }
                    ExecutionContext::Materialized(v) => {
                        let materialized = &candidates.physical_candidates[v.0];
                        match materialized.chain_tumbling_window_width_ms(candidates) {
                            Some(width_ms) => (width_ms, 0),
                            None => match logical.evaluation_grid_of_group
                                [materialized.logical_candidate_id.group.0 as usize]
                            {
                                Some(grid) => (grid.period_ms, grid.phase_ms),
                                None => return false,
                            },
                        }
                    }
                };
                match chain_tumbling_window_width_ms {
                    Some(width_ms) => {
                        period_ms.is_multiple_of(width_ms) && phase_ms.is_multiple_of(width_ms)
                    }
                    None => {
                        logical.evaluation_grid_of_group[group.0 as usize].is_some_and(|grid| {
                            period_ms.is_multiple_of(grid.period_ms)
                                && phase_ms % grid.period_ms == grid.phase_ms % grid.period_ms
                        })
                    }
                }
            }
        }
    }

    /// Its tumbling-window width when it is a chain
    /// (`SummaryTumblingWindowChain`).
    pub fn chain_tumbling_window_width_ms(
        &self,
        candidates: &PhysicalASAPMemoCandidates,
    ) -> Option<u64> {
        match candidates
            .logical_candidates
            .memo
            .candidate(self.logical_candidate_id)
            .expression
            .operator
        {
            Operator::ASAP(ASAPOp::SummaryTumblingWindowChain {
                tumbling_window_width_ms,
                ..
            }) => Some(tumbling_window_width_ms),
            _ => None,
        }
    }
}

/// $\alpha^{\mathrm{phys}}_{u,c,i}$: timing and lifetime only (the concepts
/// doc's table, section 2). Alignment is checked per context in Stage 3 with
/// [`PhysicalCandidate::delivers_on`].
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
