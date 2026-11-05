//! A physical candidate's physical properties (#1200): when it runs
//! ([`ExecutionTiming`]) and how long its output lives ([`Lifetime`]).
//!
//! | timing | lifetime | how it runs |
//! |---|---|---|
//! | query | `None` | in each evaluation that reads it; discarded after its reader |
//! | query | `OneEvaluation` | once per evaluation; its readers in that evaluation share it |
//! | query | `AcrossEvaluations` | at query time, materialized for later evaluations (a tumbling-window chain that builds its newest window each evaluation) |
//! | ingestion | `None` | as data arrives, inside the ingestion work that reads it |
//! | ingestion | `AcrossEvaluations` | as data arrives; materialized until a query reads it |
//! | ingestion | `OneEvaluation` | invalid: there is no evaluation at ingestion time |

use crate::ir::properties::ExecutionTiming;

/// Where a materialized output is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Medium {
    Memory,
    Disk,
}

/// How long a node's output lives. Only a materialized output has a medium.
#[derive(
    Default, Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
pub enum Lifetime {
    /// Discarded after its reader's execution completes.
    #[default]
    None,
    /// Materialized until every reader within one evaluation finishes.
    OneEvaluation(Medium),
    /// Materialized across evaluations, so later evaluations reuse it.
    AcrossEvaluations(Medium),
}

impl Lifetime {
    /// Whether the output outlives its reader: `OneEvaluation` or
    /// `AcrossEvaluations`.
    pub fn is_materialized(self) -> bool {
        self != Self::None
    }

    /// Whether the output is materialized across evaluations: a materialized view
    /// that later evaluations reuse.
    pub fn is_across_evaluations(self) -> bool {
        matches!(self, Self::AcrossEvaluations(_))
    }
}

/// A timing and a lifetime that cannot go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPhysicalProperties {
    #[error("an ingestion-time output cannot be materialized for one evaluation: there is no evaluation at ingestion time")]
    OneEvaluationAtIngestion,
}

/// When a candidate runs and how long its output lives: one of the valid
/// rows of the table in this module's documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalProperties {
    timing: ExecutionTiming,
    lifetime: Lifetime,
}

impl PhysicalProperties {
    /// Query time, not materialized: runs in each reader's execution.
    pub const NOT_MATERIALIZED_AT_QUERY_TIME: Self = Self {
        timing: ExecutionTiming::QueryTime,
        lifetime: Lifetime::None,
    };
    /// Ingestion time, not materialized: runs inside the ingestion work that reads it.
    pub const NOT_MATERIALIZED_AT_INGESTION: Self = Self {
        timing: ExecutionTiming::IngestionTime,
        lifetime: Lifetime::None,
    };
    /// Query time, materialized in memory until the evaluation's readers finish.
    pub const MATERIALIZED_IN_MEMORY_FOR_ONE_EVALUATION: Self = Self {
        timing: ExecutionTiming::QueryTime,
        lifetime: Lifetime::OneEvaluation(Medium::Memory),
    };
    /// Ingestion time, materialized in memory until a query reads it.
    pub const MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION: Self = Self {
        timing: ExecutionTiming::IngestionTime,
        lifetime: Lifetime::AcrossEvaluations(Medium::Memory),
    };
    /// Query time, materialized in memory across evaluations (a tumbling-window chain
    /// that builds its newest window at each evaluation).
    pub const MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_QUERY_TIME: Self = Self {
        timing: ExecutionTiming::QueryTime,
        lifetime: Lifetime::AcrossEvaluations(Medium::Memory),
    };

    /// The properties `timing` and `lifetime`, unless they cannot go together.
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> Result<Self, InvalidPhysicalProperties> {
        match (timing, lifetime) {
            (ExecutionTiming::IngestionTime, Lifetime::OneEvaluation(_)) => {
                Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
            }
            _ => Ok(Self { timing, lifetime }),
        }
    }

    pub fn timing(self) -> ExecutionTiming {
        self.timing
    }

    pub fn lifetime(self) -> Lifetime {
        self.lifetime
    }

    /// Whether a reader running at `reader` may read this output: one at the
    /// same timing, or a query-time reader of an ingestion-time output materialized
    /// until a query reads it.
    pub fn readable_at(self, reader: ExecutionTiming) -> bool {
        reader == self.timing
            || (reader == ExecutionTiming::QueryTime
                && self.timing == ExecutionTiming::IngestionTime
                && self.lifetime.is_across_evaluations())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_evaluation_at_ingestion_is_invalid() {
        assert_eq!(
            PhysicalProperties::new(
                ExecutionTiming::IngestionTime,
                Lifetime::OneEvaluation(Medium::Memory)
            ),
            Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
        );
    }

    #[test]
    fn every_other_pair_is_valid() {
        for timing in [ExecutionTiming::QueryTime, ExecutionTiming::IngestionTime] {
            for lifetime in [
                Lifetime::None,
                Lifetime::OneEvaluation(Medium::Memory),
                Lifetime::AcrossEvaluations(Medium::Disk),
            ] {
                let valid = !(timing == ExecutionTiming::IngestionTime
                    && matches!(lifetime, Lifetime::OneEvaluation(_)));
                assert_eq!(PhysicalProperties::new(timing, lifetime).is_ok(), valid);
            }
        }
    }

    /// The named properties are valid pairs.
    #[test]
    fn named_properties_are_valid() {
        for named in [
            PhysicalProperties::NOT_MATERIALIZED_AT_QUERY_TIME,
            PhysicalProperties::NOT_MATERIALIZED_AT_INGESTION,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_FOR_ONE_EVALUATION,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION,
            PhysicalProperties::MATERIALIZED_IN_MEMORY_ACROSS_EVALUATIONS_AT_QUERY_TIME,
        ] {
            assert_eq!(
                PhysicalProperties::new(named.timing(), named.lifetime()),
                Ok(named)
            );
        }
    }

    #[test]
    fn a_query_time_reader_reads_only_a_materialized_ingestion_output() {
        let at = |timing, lifetime| PhysicalProperties::new(timing, lifetime).unwrap();
        let query = ExecutionTiming::QueryTime;
        let ingestion = ExecutionTiming::IngestionTime;
        assert!(at(ingestion, Lifetime::AcrossEvaluations(Medium::Memory)).readable_at(query));
        assert!(!at(ingestion, Lifetime::None).readable_at(query));
        assert!(at(ingestion, Lifetime::None).readable_at(ingestion));
        assert!(!at(query, Lifetime::OneEvaluation(Medium::Memory)).readable_at(ingestion));
        assert!(at(query, Lifetime::AcrossEvaluations(Medium::Memory)).readable_at(query));
    }
}
