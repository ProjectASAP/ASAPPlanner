//! A physical candidate's physical properties (#1200): when it runs
//! ([`ExecutionTiming`]) and how long its output lives ([`Lifetime`]).
//!
//! | timing | lifetime | how it runs |
//! |---|---|---|
//! | query | `None` | in each evaluation that reads it; discarded after its reader |
//! | query | `OneEvaluation` | once per evaluation; its readers in that evaluation share it |
//! | query | `AcrossEvaluations` | at query time, kept for later evaluations (a tumbling-window chain that builds its newest window each evaluation) |
//! | ingestion | `None` | as data arrives, inside the ingestion work that reads it |
//! | ingestion | `AcrossEvaluations` | as data arrives; kept until a query reads it |
//! | ingestion | `OneEvaluation` | invalid: there is no evaluation at ingestion time |

use crate::ir::properties::ExecutionTiming;

/// Where a kept output is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Medium {
    Memory,
    Disk,
}

/// How long a node's output lives. Only a kept output has a medium.
#[derive(
    Default, Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Lifetime {
    /// Discarded after its reader's execution completes.
    #[default]
    None,
    /// Kept until every reader within one evaluation finishes.
    OneEvaluation(Medium),
    /// Kept across evaluations, so later evaluations reuse it.
    AcrossEvaluations(Medium),
}

impl Lifetime {
    /// Whether the output outlives its reader: `OneEvaluation` or
    /// `AcrossEvaluations`.
    pub fn is_kept(self) -> bool {
        self != Self::None
    }

    /// Whether the output is kept across evaluations: a materialized view
    /// that later evaluations reuse.
    pub fn is_across_evaluations(self) -> bool {
        matches!(self, Self::AcrossEvaluations(_))
    }
}

/// A timing and a lifetime that cannot go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPhysicalProperties {
    #[error("an ingestion-time output cannot be kept for one evaluation: there is no evaluation at ingestion time")]
    OneEvaluationAtIngestion,
}

/// When a candidate runs and how long its output lives: one of the valid
/// rows of the table in this module's documentation, each a variant, so an
/// invalid pair cannot be built or matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalProperties {
    /// Ingestion time, not kept: runs inside the ingestion work that reads it.
    NotKeptAtIngestion,
    /// Ingestion time, kept until a query reads it.
    KeptAcrossEvaluationsAtIngestion(Medium),
    /// Query time, not kept: runs in each reader's execution.
    NotKeptAtQueryTime,
    /// Query time, kept until the evaluation's readers finish.
    KeptForOneEvaluationAtQueryTime(Medium),
    /// Query time, kept across evaluations (a tumbling-window chain that
    /// builds its newest window at each evaluation).
    KeptAcrossEvaluationsAtQueryTime(Medium),
}

impl PhysicalProperties {
    /// Query time, kept in memory until the evaluation's readers finish.
    pub const KEPT_IN_MEMORY_FOR_ONE_EVALUATION: Self =
        Self::KeptForOneEvaluationAtQueryTime(Medium::Memory);
    /// Ingestion time, kept in memory until a query reads it.
    pub const KEPT_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION: Self =
        Self::KeptAcrossEvaluationsAtIngestion(Medium::Memory);
    /// Query time, kept in memory across evaluations.
    pub const KEPT_IN_MEMORY_ACROSS_EVALUATIONS_AT_QUERY_TIME: Self =
        Self::KeptAcrossEvaluationsAtQueryTime(Medium::Memory);

    /// The properties `timing` and `lifetime`, unless they cannot go together.
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> Result<Self, InvalidPhysicalProperties> {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (timing, lifetime) {
            (IngestionTime, Lifetime::None) => Ok(Self::NotKeptAtIngestion),
            (IngestionTime, Lifetime::OneEvaluation(_)) => {
                Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
            }
            (IngestionTime, Lifetime::AcrossEvaluations(medium)) => {
                Ok(Self::KeptAcrossEvaluationsAtIngestion(medium))
            }
            (QueryTime, Lifetime::None) => Ok(Self::NotKeptAtQueryTime),
            (QueryTime, Lifetime::OneEvaluation(medium)) => {
                Ok(Self::KeptForOneEvaluationAtQueryTime(medium))
            }
            (QueryTime, Lifetime::AcrossEvaluations(medium)) => {
                Ok(Self::KeptAcrossEvaluationsAtQueryTime(medium))
            }
        }
    }

    pub fn timing(self) -> ExecutionTiming {
        match self {
            Self::NotKeptAtIngestion | Self::KeptAcrossEvaluationsAtIngestion(_) => {
                ExecutionTiming::IngestionTime
            }
            Self::NotKeptAtQueryTime
            | Self::KeptForOneEvaluationAtQueryTime(_)
            | Self::KeptAcrossEvaluationsAtQueryTime(_) => ExecutionTiming::QueryTime,
        }
    }

    pub fn lifetime(self) -> Lifetime {
        match self {
            Self::NotKeptAtIngestion | Self::NotKeptAtQueryTime => Lifetime::None,
            Self::KeptForOneEvaluationAtQueryTime(medium) => Lifetime::OneEvaluation(medium),
            Self::KeptAcrossEvaluationsAtIngestion(medium)
            | Self::KeptAcrossEvaluationsAtQueryTime(medium) => Lifetime::AcrossEvaluations(medium),
        }
    }

    /// Whether a reader running at `reader` may read this output: one at the
    /// same timing, or a query-time reader of an ingestion-time output kept
    /// until a query reads it.
    pub fn readable_at(self, reader: ExecutionTiming) -> bool {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (self, reader) {
            (Self::NotKeptAtIngestion, IngestionTime) => true,
            // A query reads an ingestion-time output only where it is kept.
            (Self::NotKeptAtIngestion, QueryTime) => false,
            (Self::KeptAcrossEvaluationsAtIngestion(_), IngestionTime | QueryTime) => true,
            // Ingestion work never reads a query-time output.
            (
                Self::NotKeptAtQueryTime
                | Self::KeptForOneEvaluationAtQueryTime(_)
                | Self::KeptAcrossEvaluationsAtQueryTime(_),
                IngestionTime,
            ) => false,
            (
                Self::NotKeptAtQueryTime
                | Self::KeptForOneEvaluationAtQueryTime(_)
                | Self::KeptAcrossEvaluationsAtQueryTime(_),
                QueryTime,
            ) => true,
        }
    }

    /// Every valid pair: each timing with each lifetime and medium, less
    /// `(IngestionTime, OneEvaluation)`. For table tests over every state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn every_valid() -> Vec<Self> {
        let lifetimes = [
            Lifetime::None,
            Lifetime::OneEvaluation(Medium::Memory),
            Lifetime::OneEvaluation(Medium::Disk),
            Lifetime::AcrossEvaluations(Medium::Memory),
            Lifetime::AcrossEvaluations(Medium::Disk),
        ];
        [ExecutionTiming::IngestionTime, ExecutionTiming::QueryTime]
            .into_iter()
            .flat_map(|timing| lifetimes.map(|lifetime| Self::new(timing, lifetime)))
            .filter_map(Result::ok)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two invalid pairs are rejected; the eight others are valid.
    #[test]
    fn exactly_the_ingestion_time_one_evaluation_pairs_are_invalid() {
        for medium in [Medium::Memory, Medium::Disk] {
            assert_eq!(
                PhysicalProperties::new(
                    ExecutionTiming::IngestionTime,
                    Lifetime::OneEvaluation(medium)
                ),
                Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
            );
        }
        assert_eq!(PhysicalProperties::every_valid().len(), 8);
    }

    /// `readable_at` for every valid state and both reader timings.
    #[test]
    fn readable_at_over_every_valid_state() {
        use ExecutionTiming::{IngestionTime, QueryTime};
        for properties in PhysicalProperties::every_valid() {
            let expected_at_query = match properties {
                PhysicalProperties::NotKeptAtIngestion => false,
                PhysicalProperties::KeptAcrossEvaluationsAtIngestion(_)
                | PhysicalProperties::NotKeptAtQueryTime
                | PhysicalProperties::KeptForOneEvaluationAtQueryTime(_)
                | PhysicalProperties::KeptAcrossEvaluationsAtQueryTime(_) => true,
            };
            assert_eq!(
                properties.readable_at(QueryTime),
                expected_at_query,
                "{properties:?}"
            );
            assert_eq!(
                properties.readable_at(IngestionTime),
                properties.timing() == IngestionTime,
                "{properties:?}"
            );
        }
    }

    /// The named properties are valid pairs.
    #[test]
    fn named_properties_are_valid() {
        for named in [
            PhysicalProperties::KEPT_IN_MEMORY_FOR_ONE_EVALUATION,
            PhysicalProperties::KEPT_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
            PhysicalProperties::KEPT_IN_MEMORY_ACROSS_EVALUATIONS_AT_QUERY_TIME,
        ] {
            assert_eq!(
                PhysicalProperties::new(named.timing(), named.lifetime()),
                Ok(named)
            );
        }
    }
}
