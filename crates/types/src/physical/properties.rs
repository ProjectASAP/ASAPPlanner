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
}

/// A timing and a lifetime that cannot go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPhysicalProperties {
    #[error("an ingestion-time output cannot be kept for one evaluation: there is no evaluation at ingestion time")]
    OneEvaluationAtIngestion,
}

/// When a candidate runs and how long its output lives: one of the valid
/// rows of the table in this module's documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub struct PhysicalProperties {
    timing: ExecutionTiming,
    lifetime: Lifetime,
}

impl PhysicalProperties {
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
    /// same timing, or a query-time reader of an ingestion-time output kept
    /// until a query reads it.
    pub fn readable_at(self, reader: ExecutionTiming) -> bool {
        reader == self.timing
            || (reader == ExecutionTiming::QueryTime
                && self.timing == ExecutionTiming::IngestionTime
                && matches!(self.lifetime, Lifetime::AcrossEvaluations(_)))
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

    #[test]
    fn a_query_time_reader_reads_only_a_kept_ingestion_output() {
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
