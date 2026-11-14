//! Candidate scope: each logical candidate's physical alternatives, one per
//! valid $p^{\mathrm{phys}} = (\tau, \ell)$.

use crate::ir::properties::ExecutionTiming;
use crate::stages::{CandidateForMemoGroupId, ExecutionContext, PhysicalASAPMemoCandidates};
use crate::stages::{Lifetime, PhysicalProperties};
use crate::workload::DataArrival;

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
    /// phase. Any other result is stored on its group's evaluation grid
    /// (`evaluation_grid_of_group`), which must hold every time of the
    /// context, unless it is final: the same at every time it is read.
    ///
    /// Finality assumes no row arrives out of order: at evaluation time t,
    /// every row with `ts <= t` has arrived and never changes (#1298 adds
    /// a declared lateness). A stored result is final when its group does
    /// not depend on the evaluation time
    /// ([`GroupProperties::depends_on_evaluation_time`](crate::stages::GroupProperties::depends_on_evaluation_time))
    /// and the rows it read never change after it is computed:
    ///
    /// | Result | Delivers on |
    /// |---|---|
    /// | kept up to date at ingestion time (`IngestionTime`) | every context |
    /// | over data at rest (`DataArrival::AtRest`) or reading no source | every context |
    /// | at query time, reading a time relative to its evaluation (`rate(x[5m])`, `@ end()`, `now()`) | its own grid |
    /// | at query time over arriving rows, no time bound (SQL `count(*)`) | its own grid: never final |
    /// | at query time over arriving rows, a window ending at E (`sum_over_time(x[5m] @ T)`, `ts BETWEEN a AND b`) | its own grid: final only from E on, and a context carries no absolute time to compare with E |
    ///
    /// `DataArrival::Unknown` counts as arriving.
    /// The times of a
    /// `Materialized(v)` context are $v$'s own
    /// ([`LogicalASAPMemoCandidates::own_schedule`](crate::stages::LogicalASAPMemoCandidates::own_schedule));
    /// when they are not known, no non-final producer delivers on it.
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
                let group_properties = &logical.memo.groups()[group.0 as usize].group_properties;
                let final_at_every_time = !group_properties.depends_on_evaluation_time
                    && (self.physical_properties.timing() == ExecutionTiming::IngestionTime
                        || logical.memo.data_arrival() == DataArrival::AtRest
                        || group_properties.scanned_sources.is_empty());
                if logical
                    .memo
                    .candidate(self.logical_candidate_id)
                    .expression
                    .operator
                    .chain_tumbling_window_width_ms()
                    .is_none()
                    && final_at_every_time
                {
                    return true;
                }
                let (period_ms, phase_ms) = match context {
                    ExecutionContext::Evaluation { grid } => (grid.period_ms, grid.phase_ms),
                    ExecutionContext::IngestionPipeline(window) => {
                        (window.width_ms, window.phase_ms)
                    }
                    ExecutionContext::Materialized(v) => {
                        let materialized = candidates.physical_candidates[v.0].logical_candidate_id;
                        match logical.own_schedule(materialized) {
                            Some(grid) => (grid.period_ms, grid.phase_ms),
                            None => return false,
                        }
                    }
                };
                logical
                    .own_schedule(self.logical_candidate_id)
                    .is_some_and(|stored| {
                        stored.period_ms > 0
                            && period_ms.is_multiple_of(stored.period_ms)
                            && phase_ms % stored.period_ms == stored.phase_ms % stored.period_ms
                    })
            }
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
