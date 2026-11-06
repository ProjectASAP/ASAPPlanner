//! $\kappa$: where a candidate with $\ell$ = None or `OneEvaluation` runs
//! (concepts doc, section 5). Stage 3 indexes $x_{\kappa,v}$ and
//! $y_{\kappa,u,c,i}$ by it; [`super::PhysicalCandidate::delivers_on`] reads
//! it, so it lives with the physical candidates.

use super::graph::PhysicalCandidateId;
use crate::workload::{QueryRecurrence, RangeEvaluation};

/// A refresh window of the workload's ingestion pipelines, by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RefreshWindowId(pub usize);

/// Index of an [`ExecutionContext`] in the workload's list of distinct
/// contexts. Contexts are deduplicated by equality: [`QueryRecurrence`]
/// holds an `f64` (`EstimatedRate`), so a context cannot be hashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExecutionContextId(pub usize);

/// $\kappa$: a class of executions with a common owner and schedule, where
/// a candidate with $\ell$ = None or `OneEvaluation` runs.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionContext {
    /// The evaluation of every query with this recurrence and range
    /// evaluation, the workload's own description of when it runs: queries
    /// with equal ones are evaluated together and share its `OneEvaluation`
    /// results. A and B both every 10 s at phase 0 give one context; a
    /// one-off query's context is its `OneTime` recurrence.
    Evaluation {
        recurrence: QueryRecurrence,
        range_evaluation: Option<RangeEvaluation>,
    },
    /// $I_r$: the ingestion pipeline of a refresh window.
    IngestionPipeline(RefreshWindowId),
    /// $\kappa_m$: inside an `AcrossEvaluations` candidate's own computation.
    Materialized(PhysicalCandidateId),
}
