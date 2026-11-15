//! Stage 2's output: each logical candidate's physical alternatives
//! $\mathcal C$, one per valid $p^{\mathrm{phys}} = (\tau, \ell)$, and
//! the physical edges $\mathcal E$ with $\alpha^{\log}$ and
//! $\alpha^{\mathrm{phys}}$. Stage 3 selects among them;
//! Stage 2 prices nothing.

use std::collections::HashMap;

use crate::ir::properties::ExecutionTiming;
use crate::ir::{NonASAPOp, Operator, QueryRoot};
use crate::plan::materialized_view::RefreshWindow;
use crate::plan::time_window::{InputTimeAccess, TimeIndexedPlan};
use crate::stages::{
    EvaluationGrid, ExecutionContext, Lifetime, LogicalASAPMemoCandidates, MemoGroupId,
    PhysicalCandidate, PhysicalCandidateId, PhysicalEdge, PhysicalEdgeId, PrunedCandidate,
};
use crate::workload::DataArrival;

/// Stage 2's output, which Stage 3 selects from.
#[derive(Debug, Clone)]
pub struct PhysicalASAPMemoCandidates {
    /// Pass 2's output: the memo, its candidates' $e_v$, $p^{\log}_v$,
    /// $r^{\log}_{c,i}$, and the logical edges.
    pub logical_candidates: LogicalASAPMemoCandidates,
    /// $\mathcal C$, indexed by [`PhysicalCandidateId`].
    pub physical_candidates: Vec<PhysicalCandidate>,
    /// The alternatives Stage 2 pruned, in pruning order, each as the
    /// [`PhysicalCandidate`] it would have been: candidates are numbered
    /// after pruning, so a pruned one has no [`PhysicalCandidateId`].
    pub pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    /// $\mathcal E$, grouped by (consumer, input); indexed by
    /// [`PhysicalEdgeId`].
    edges: Vec<PhysicalEdge>,
    /// $P_{c,i}$ after $\alpha$: the admissible edges into each (consumer, input).
    admissible_edges_into_input: HashMap<(PhysicalCandidateId, usize), Vec<PhysicalEdgeId>>,
    /// The admissible edges out of each candidate, by [`PhysicalCandidateId`].
    admissible_edges_out_of_candidate: Vec<Vec<PhysicalEdgeId>>,
}

impl PhysicalASAPMemoCandidates {
    /// Stage 2's output over `physical_candidates` and `edges` (grouped by
    /// (consumer, input)), with the admissible-edge indexes built once.
    /// Query roots are read from `logical_candidates`, not copied.
    pub fn new(
        logical_candidates: LogicalASAPMemoCandidates,
        physical_candidates: Vec<PhysicalCandidate>,
        edges: Vec<PhysicalEdge>,
        pruned_candidates: Vec<PrunedCandidate<PhysicalCandidate>>,
    ) -> Self {
        let mut admissible_edges_into_input: HashMap<_, Vec<PhysicalEdgeId>> = HashMap::new();
        let mut admissible_edges_out_of_candidate = vec![Vec::new(); physical_candidates.len()];
        for (id, edge) in edges.iter().enumerate() {
            if edge.is_logically_and_physically_admissible() {
                admissible_edges_into_input
                    .entry((edge.consumer_candidate_id, edge.input_index_of_consumer))
                    .or_default()
                    .push(PhysicalEdgeId(id));
                admissible_edges_out_of_candidate[edge.producer_candidate_id.0]
                    .push(PhysicalEdgeId(id));
            }
        }
        Self {
            logical_candidates,
            physical_candidates,
            pruned_candidates,
            edges,
            admissible_edges_into_input,
            admissible_edges_out_of_candidate,
        }
    }

    /// $\mathcal E$: every edge, in (consumer, input) order.
    pub fn edges(&self) -> impl Iterator<Item = (PhysicalEdgeId, PhysicalEdge)> + '_ {
        self.edges
            .iter()
            .enumerate()
            .map(|(id, edge)| (PhysicalEdgeId(id), *edge))
    }

    /// Edge `id` of $\mathcal E$.
    pub fn edge(&self, id: PhysicalEdgeId) -> &PhysicalEdge {
        &self.edges[id.0]
    }

    /// The operator of candidate `v`'s logical candidate.
    pub fn operator(&self, v: PhysicalCandidateId) -> &Operator<MemoGroupId> {
        &self
            .logical_candidates
            .memo
            .candidate(self.physical_candidates[v.0].logical_candidate_id)
            .expression
            .operator
    }

    /// The number of inputs of candidate `v`: its logical candidate's
    /// operator's children (e.g. 2 for a binary operator).
    pub fn input_count(&self, v: PhysicalCandidateId) -> usize {
        self.operator(v).children().len()
    }

    /// $P^{\mathrm{adm}}_{c,i}$: the admissible edges into input
    /// `input_index_of_consumer` of `consumer`
    /// ($\alpha^{\log} = \alpha^{\mathrm{phys}} = 1$).
    pub fn admissible_edges_into(
        &self,
        consumer: PhysicalCandidateId,
        input_index_of_consumer: usize,
    ) -> &[PhysicalEdgeId] {
        self.admissible_edges_into_input
            .get(&(consumer, input_index_of_consumer))
            .map_or(&[], Vec::as_slice)
    }

    /// The admissible edges out of `producer`: every consumer that may share it.
    pub fn admissible_edges_out_of(&self, producer: PhysicalCandidateId) -> &[PhysicalEdgeId] {
        &self.admissible_edges_out_of_candidate[producer.0]
    }

    /// The times candidate `v`, materialized across evaluations, has a
    /// stored result for, as a grid: at ingestion time the end of every
    /// refresh window, by the rule the installed plan refreshes it on
    /// ([`RefreshWindow::computed_from`] over these candidates, width as the
    /// period); at query time its own schedule
    /// ([`LogicalASAPMemoCandidates::own_schedule`]). `None` when not known.
    ///
    /// Example: `sum(TimeRange(5m))` kept at ingestion time is stored every
    /// 5 min at phase 0, even when its group is read every 60 s; a chain of
    /// 60 s windows over `TimeShift(30s)` every 60 s at phase 30 s.
    pub fn stored_times(&self, v: PhysicalCandidateId) -> Option<EvaluationGrid> {
        let candidate = &self.physical_candidates[v.0];
        match candidate.physical_properties.timing() {
            ExecutionTiming::IngestionTime => {
                RefreshWindow::computed_from(self, v).map(|window| EvaluationGrid {
                    period_ms: window.width_ms,
                    phase_ms: window.phase_ms,
                })
            }
            ExecutionTiming::QueryTime => self
                .logical_candidates
                .own_schedule(candidate.logical_candidate_id),
        }
    }

    /// Whether a consumer running in `context` can read producer `v` (the
    /// concepts doc's alignment table, section 2): true for $\ell$ = None,
    /// which runs in the consumer's own context; for `OneEvaluation` only in
    /// an `Evaluation` context, the one evaluation its result belongs to.
    /// With $\ell$ = `AcrossEvaluations`, every execution time of the
    /// context must be a time the producer has a stored result for
    /// ([`Self::stored_times`]): its period divides the context's period (or
    /// refresh-window width) and the phases agree modulo it, unless the
    /// result is final: the same at every time it is read.
    ///
    /// Finality assumes no row arrives out of order: at evaluation time t,
    /// every row with `ts <= t` has arrived and never changes (#1298 adds
    /// a declared lateness). A stored result that is not a chain is final
    /// when its group does not depend on the evaluation time
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
    /// `DataArrival::Unknown` counts as arriving. The times of a
    /// `Materialized(u)` context are $u$'s stored times; when they are not
    /// known, no non-final producer delivers on it.
    ///
    /// Example: `sum(TimeRange(5m))` kept at ingestion time delivers on an
    /// evaluation grid every 5 min, not on one every 60 s.
    pub fn delivers_on(&self, v: PhysicalCandidateId, context: &ExecutionContext) -> bool {
        let producer = &self.physical_candidates[v.0];
        match producer.physical_properties.lifetime() {
            Lifetime::None => true,
            Lifetime::OneEvaluation(_) => matches!(context, ExecutionContext::Evaluation { .. }),
            Lifetime::AcrossEvaluations(_) => {
                let memo = &self.logical_candidates.memo;
                let group_properties =
                    &memo.groups()[producer.logical_candidate_id.group.0 as usize].group_properties;
                let final_at_every_time = !group_properties.depends_on_evaluation_time
                    && (producer.physical_properties.timing() == ExecutionTiming::IngestionTime
                        || memo.data_arrival() == DataArrival::AtRest
                        || group_properties.scanned_sources.is_empty());
                if self.operator(v).chain_tumbling_window_width_ms().is_none()
                    && final_at_every_time
                {
                    return true;
                }
                let (period_ms, phase_ms) = match context {
                    ExecutionContext::Evaluation { grid } => (grid.period_ms, grid.phase_ms),
                    ExecutionContext::IngestionPipeline(window) => {
                        (window.width_ms, window.phase_ms)
                    }
                    ExecutionContext::Materialized(u) => match self.stored_times(*u) {
                        Some(grid) => (grid.period_ms, grid.phase_ms),
                        None => return false,
                    },
                };
                self.stored_times(v).is_some_and(|stored| {
                    stored.period_ms > 0
                        && period_ms.is_multiple_of(stored.period_ms)
                        && phase_ms % stored.period_ms == stored.phase_ms % stored.period_ms
                })
            }
        }
    }
}

/// The candidates as the time walks see them: each candidate's admissible
/// producers as its inputs and admissible consumers as its readers, its
/// operator at its timing and lifetime ([`InputTimeAccess::of`]), and a
/// current-time read of a `Scan` (raw data) needing every row. The
/// producers of one input are alternatives, so a walk down over them
/// covers every plan that may select them.
impl TimeIndexedPlan for PhysicalASAPMemoCandidates {
    type Node = PhysicalCandidateId;
    type Child = MemoGroupId;

    fn readers_of(&self, v: PhysicalCandidateId) -> Vec<PhysicalCandidateId> {
        self.admissible_edges_out_of(v)
            .iter()
            .map(|&out| self.edge(out).consumer_candidate_id)
            .collect()
    }

    fn inputs_of(&self, v: PhysicalCandidateId) -> Vec<PhysicalCandidateId> {
        (0..self.input_count(v))
            .flat_map(|input| self.admissible_edges_into(v, input))
            .map(|&edge| self.edge(edge).producer_candidate_id)
            .collect()
    }

    fn input_time_access(&self, v: PhysicalCandidateId) -> InputTimeAccess {
        let properties = self.physical_candidates[v.0].physical_properties;
        InputTimeAccess::of(self.operator(v), properties.timing(), properties.lifetime())
    }

    fn current_time_read_needs_every_row(&self, v: PhysicalCandidateId) -> bool {
        matches!(self.operator(v), Operator::NonASAP(NonASAPOp::Scan { .. }))
    }

    fn operator_of(&self, v: PhysicalCandidateId) -> &Operator<MemoGroupId> {
        self.operator(v)
    }
}

/// The output references of `query_roots`: each operator reference, in
/// order (a scalar root's in `map_operator_refs` order), with the index of
/// the query whose root it is.
pub fn output_references<C: Clone>(query_roots: &[QueryRoot<C>]) -> Vec<(usize, C)> {
    let mut references = Vec::new();
    for (query, root) in query_roots.iter().enumerate() {
        match root {
            QueryRoot::Operator(reference) => references.push((query, reference.clone())),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |reference| {
                    references.push((query, reference.clone()))
                });
            }
        }
    }
    references
}

#[cfg(test)]
mod tests;
