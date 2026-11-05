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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
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

/// When a candidate runs and how long its output lives: not kept at a
/// timing, or one of the kept states. Each valid row of the table in this
/// module's documentation is one value, so an invalid pair cannot be built
/// or matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalProperties {
    /// Not kept: runs inside the execution that reads it (each reader's at
    /// query time, the ingestion work that reads it at ingestion time).
    NotKept(ExecutionTiming),
    /// Kept beyond its reader.
    Kept(KeptProperties),
}

/// A kept output: where and how long it is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeptProperties {
    /// Ingestion time, kept until a query reads it.
    AcrossEvaluationsAtIngestion(Medium),
    /// Query time, kept until the evaluation's readers finish.
    ForOneEvaluationAtQueryTime(Medium),
    /// Query time, kept across evaluations (a tumbling-window chain that
    /// builds its newest window at each evaluation).
    AcrossEvaluationsAtQueryTime(Medium),
}

impl KeptProperties {
    /// The states Stage 2 offers: every kept state in memory (no disk store
    /// in the executor yet, #1217).
    pub const IN_MEMORY_FOR_ONE_EVALUATION: Self =
        Self::ForOneEvaluationAtQueryTime(Medium::Memory);
    pub const IN_MEMORY_ACROSS_EVALUATIONS_AT_INGESTION: Self =
        Self::AcrossEvaluationsAtIngestion(Medium::Memory);
    pub const IN_MEMORY_ACROSS_EVALUATIONS_AT_QUERY_TIME: Self =
        Self::AcrossEvaluationsAtQueryTime(Medium::Memory);

    pub fn timing(self) -> ExecutionTiming {
        match self {
            Self::AcrossEvaluationsAtIngestion(_) => ExecutionTiming::IngestionTime,
            Self::ForOneEvaluationAtQueryTime(_) | Self::AcrossEvaluationsAtQueryTime(_) => {
                ExecutionTiming::QueryTime
            }
        }
    }

    pub fn lifetime(self) -> Lifetime {
        match self {
            Self::ForOneEvaluationAtQueryTime(medium) => Lifetime::OneEvaluation(medium),
            Self::AcrossEvaluationsAtIngestion(medium)
            | Self::AcrossEvaluationsAtQueryTime(medium) => Lifetime::AcrossEvaluations(medium),
        }
    }
}

impl PhysicalProperties {
    /// The properties `timing` and `lifetime`, unless they cannot go together.
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> Result<Self, InvalidPhysicalProperties> {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (timing, lifetime) {
            (_, Lifetime::None) => Ok(Self::NotKept(timing)),
            (IngestionTime, Lifetime::OneEvaluation(_)) => {
                Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
            }
            (IngestionTime, Lifetime::AcrossEvaluations(medium)) => Ok(Self::Kept(
                KeptProperties::AcrossEvaluationsAtIngestion(medium),
            )),
            (QueryTime, Lifetime::OneEvaluation(medium)) => Ok(Self::Kept(
                KeptProperties::ForOneEvaluationAtQueryTime(medium),
            )),
            (QueryTime, Lifetime::AcrossEvaluations(medium)) => Ok(Self::Kept(
                KeptProperties::AcrossEvaluationsAtQueryTime(medium),
            )),
        }
    }

    pub fn timing(self) -> ExecutionTiming {
        match self {
            Self::NotKept(timing) => timing,
            Self::Kept(kept) => kept.timing(),
        }
    }

    pub fn lifetime(self) -> Lifetime {
        match self {
            Self::NotKept(_) => Lifetime::None,
            Self::Kept(kept) => kept.lifetime(),
        }
    }

    /// How it is kept, if it is.
    pub fn kept(self) -> Option<KeptProperties> {
        match self {
            Self::NotKept(_) => None,
            Self::Kept(kept) => Some(kept),
        }
    }

    /// Whether a reader running at `reader` may read this output: one at the
    /// same timing, or a query-time reader of an ingestion-time output kept
    /// until a query reads it.
    pub fn readable_at(self, reader: ExecutionTiming) -> bool {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (self, reader) {
            (Self::NotKept(timing), reader) => timing == reader,
            (
                Self::Kept(KeptProperties::AcrossEvaluationsAtIngestion(_)),
                IngestionTime | QueryTime,
            ) => true,
            // Ingestion work never reads a query-time output.
            (
                Self::Kept(
                    KeptProperties::ForOneEvaluationAtQueryTime(_)
                    | KeptProperties::AcrossEvaluationsAtQueryTime(_),
                ),
                IngestionTime,
            ) => false,
            (
                Self::Kept(
                    KeptProperties::ForOneEvaluationAtQueryTime(_)
                    | KeptProperties::AcrossEvaluationsAtQueryTime(_),
                ),
                QueryTime,
            ) => true,
        }
    }

    /// The 8 valid states, for table tests over every state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn every_valid() -> [Self; 8] {
        use ExecutionTiming::{IngestionTime, QueryTime};
        use KeptProperties::{
            AcrossEvaluationsAtIngestion, AcrossEvaluationsAtQueryTime, ForOneEvaluationAtQueryTime,
        };
        use Medium::{Disk, Memory};
        let every = [
            Self::NotKept(IngestionTime),
            Self::NotKept(QueryTime),
            Self::Kept(AcrossEvaluationsAtIngestion(Memory)),
            Self::Kept(AcrossEvaluationsAtIngestion(Disk)),
            Self::Kept(ForOneEvaluationAtQueryTime(Memory)),
            Self::Kept(ForOneEvaluationAtQueryTime(Disk)),
            Self::Kept(AcrossEvaluationsAtQueryTime(Memory)),
            Self::Kept(AcrossEvaluationsAtQueryTime(Disk)),
        ];
        // Exhaustive without wildcards: a new state fails to compile here
        // until it is listed above.
        for properties in every {
            match properties {
                Self::NotKept(IngestionTime | QueryTime)
                | Self::Kept(
                    AcrossEvaluationsAtIngestion(Memory | Disk)
                    | ForOneEvaluationAtQueryTime(Memory | Disk)
                    | AcrossEvaluationsAtQueryTime(Memory | Disk),
                ) => {}
            }
        }
        every
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
        for properties in PhysicalProperties::every_valid() {
            assert_eq!(
                PhysicalProperties::new(properties.timing(), properties.lifetime()),
                Ok(properties)
            );
        }
    }

    /// `readable_at` for every valid state and both reader timings.
    #[test]
    fn readable_at_over_every_valid_state() {
        use ExecutionTiming::{IngestionTime, QueryTime};
        for properties in PhysicalProperties::every_valid() {
            let expected_at_query = match properties {
                PhysicalProperties::NotKept(IngestionTime) => false,
                PhysicalProperties::NotKept(QueryTime) | PhysicalProperties::Kept(_) => true,
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
}
