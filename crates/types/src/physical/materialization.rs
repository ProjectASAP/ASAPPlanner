//! The materialization option of each sub-DAG (#509 §Materialization), the
//! Stage 2 decision a node's execution timing
//! ([`crate::ir::properties::ExecutionTiming`]) is derived
//! from.

use crate::ir::properties::ExecutionTiming;

/// Where a materialized output is stored (#509 §Materialization).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Medium {
    Memory,
    Disk,
}

/// The materialization option of one sub-DAG (#509 §Materialization,
/// Algorithm 3): whether its output is stored across query executions, and
/// whether it is computed at ingestion time or at query time.
#[derive(
    Default, Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Materialization {
    /// Runs at query time for each execution; its output is discarded.
    #[default]
    NotMaterialized,
    /// Runs as data arrives; its output is stored until a query reads it.
    MatAtIngestion(Medium),
    /// Runs when a query first needs it; its output is reused by the other
    /// queries of the same evaluation and discarded after it.
    MatAtQuery(Medium),
    /// Runs at query time and keeps its output across evaluations: a
    /// tumbling-pane chain whose readers evaluate together every pane
    /// width builds its newest pane at each evaluation and keeps the older
    /// ones for the next (#509 Example 4's B3). Offered only when the
    /// deployment keeps query-time results.
    MatAtQueryKeptAcrossEvaluations(Medium),
}

impl Materialization {
    /// When the sub-DAG runs: only `MatAtIngestion` runs at ingestion time.
    pub fn timing(self) -> ExecutionTiming {
        match self {
            Self::MatAtIngestion(_) => ExecutionTiming::IngestionTime,
            Self::NotMaterialized
            | Self::MatAtQuery(_)
            | Self::MatAtQueryKeptAcrossEvaluations(_) => ExecutionTiming::QueryTime,
        }
    }

    /// Whether a node of this materialization is a materialized view: its
    /// output is kept across evaluations.
    pub(crate) fn is_materialized_view(self) -> bool {
        matches!(
            self,
            Self::MatAtIngestion(_) | Self::MatAtQueryKeptAcrossEvaluations(_)
        )
    }
}
