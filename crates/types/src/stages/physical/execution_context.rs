//! $\kappa$: a class of executions that happen at the same times (concepts
//! doc, section 5). Stage 3 indexes $x_{\kappa,v}$ and $y_{\kappa,u,c,i}$ by
//! it; [`super::PhysicalCandidate::delivers_on`] reads it, so it lives with
//! the physical candidates.

use super::candidate::PhysicalCandidateId;
use crate::plan::materialized_view::RefreshWindow;

/// Index of an [`ExecutionContext`] in the workload's list of distinct
/// contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExecutionContextId(pub usize);

/// $\kappa$: within one context a computation runs once, and every consumer
/// in it shares that run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionContext {
    /// At every time of an evaluation grid, answering queries: every query
    /// evaluated on the grid, a range query's steps included. An alert
    /// every 10 s at phase 0 is the grid 10 s @ 0.
    Evaluation { grid: EvaluationGrid },
    /// As data arrives, in batches of one refresh window.
    IngestionPipeline(RefreshWindow),
    /// The own computation of a candidate materialized across evaluations,
    /// on its own schedule, whoever reads it.
    Materialized(PhysicalCandidateId),
}

/// The evaluation times `phase_ms + k · period_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvaluationGrid {
    pub period_ms: u64,
    pub phase_ms: u64,
}
