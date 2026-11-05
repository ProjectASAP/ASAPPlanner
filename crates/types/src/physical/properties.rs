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
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (timing, lifetime) {
            (IngestionTime, Lifetime::OneEvaluation(_)) => {
                Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
            }
            (QueryTime | IngestionTime, Lifetime::None)
            | (QueryTime, Lifetime::OneEvaluation(_))
            | (QueryTime | IngestionTime, Lifetime::AcrossEvaluations(_)) => {
                Ok(Self { timing, lifetime })
            }
        }
    }

    /// `(timing, None)`: not materialized, at `timing`.
    pub fn not_materialized_at(timing: ExecutionTiming) -> Self {
        Self {
            timing,
            lifetime: Lifetime::None,
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
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (self.timing, self.lifetime, reader) {
            // Runs inside the execution that reads it: only a reader at its
            // own timing.
            (QueryTime, Lifetime::None, QueryTime) => true,
            (QueryTime, Lifetime::None, IngestionTime) => false,
            (IngestionTime, Lifetime::None, IngestionTime) => true,
            (IngestionTime, Lifetime::None, QueryTime) => false,
            // Kept until a query reads it: ingestion work and queries both read it.
            (IngestionTime, Lifetime::AcrossEvaluations(_), IngestionTime | QueryTime) => true,
            // A query-time output: queries read it; ingestion work never does.
            (QueryTime, Lifetime::OneEvaluation(_), QueryTime) => true,
            (QueryTime, Lifetime::OneEvaluation(_), IngestionTime) => false,
            (QueryTime, Lifetime::AcrossEvaluations(_), QueryTime) => true,
            (QueryTime, Lifetime::AcrossEvaluations(_), IngestionTime) => false,
            (IngestionTime, Lifetime::OneEvaluation(_), _) => {
                unreachable!("rejected by PhysicalProperties::new")
            }
        }
    }

    /// The 8 valid states, as literal `(timing, lifetime)` pairs through
    /// [`Self::new`], for table tests over every state.
    #[cfg(any(test, feature = "test-support"))]
    pub fn every_valid() -> [Self; 8] {
        use ExecutionTiming::{IngestionTime, QueryTime};
        use Medium::{Disk, Memory};
        [
            (QueryTime, Lifetime::None),
            (IngestionTime, Lifetime::None),
            (QueryTime, Lifetime::OneEvaluation(Memory)),
            (QueryTime, Lifetime::OneEvaluation(Disk)),
            (QueryTime, Lifetime::AcrossEvaluations(Memory)),
            (QueryTime, Lifetime::AcrossEvaluations(Disk)),
            (IngestionTime, Lifetime::AcrossEvaluations(Memory)),
            (IngestionTime, Lifetime::AcrossEvaluations(Disk)),
        ]
        .map(|(timing, lifetime)| Self::new(timing, lifetime).expect("a valid pair"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Of every timing x lifetime pair, exactly the 8 of `every_valid` are
    /// valid, and the 2 at ingestion time for one evaluation are not.
    #[test]
    fn exactly_the_ingestion_time_one_evaluation_pairs_are_invalid() {
        use ExecutionTiming::{IngestionTime, QueryTime};
        let mut valid = Vec::new();
        for timing in [QueryTime, IngestionTime] {
            for lifetime in [
                Lifetime::None,
                Lifetime::OneEvaluation(Medium::Memory),
                Lifetime::OneEvaluation(Medium::Disk),
                Lifetime::AcrossEvaluations(Medium::Memory),
                Lifetime::AcrossEvaluations(Medium::Disk),
            ] {
                match PhysicalProperties::new(timing, lifetime) {
                    Ok(properties) => valid.push(properties),
                    Err(error) => {
                        assert_eq!(error, InvalidPhysicalProperties::OneEvaluationAtIngestion);
                        assert!(
                            timing == IngestionTime
                                && matches!(lifetime, Lifetime::OneEvaluation(_))
                        );
                    }
                }
            }
        }
        let every_valid = PhysicalProperties::every_valid();
        assert_eq!(valid.len(), every_valid.len());
        assert!(valid.iter().all(|state| every_valid.contains(state)));
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

    /// `readable_at` for every valid state and both reader timings.
    #[test]
    fn readable_at_over_every_valid_state() {
        use ExecutionTiming::{IngestionTime, QueryTime};
        let at = |timing, lifetime| PhysicalProperties::new(timing, lifetime).unwrap();
        // (state, readable by a query-time reader, readable by an ingestion-time reader)
        let mut expected = Vec::new();
        for medium in [Medium::Memory, Medium::Disk] {
            expected.extend([
                (at(QueryTime, Lifetime::OneEvaluation(medium)), true, false),
                (
                    at(QueryTime, Lifetime::AcrossEvaluations(medium)),
                    true,
                    false,
                ),
                (
                    at(IngestionTime, Lifetime::AcrossEvaluations(medium)),
                    true,
                    true,
                ),
            ]);
        }
        expected.extend([
            (at(QueryTime, Lifetime::None), true, false),
            (at(IngestionTime, Lifetime::None), false, true),
        ]);
        assert_eq!(expected.len(), PhysicalProperties::every_valid().len());
        for properties in PhysicalProperties::every_valid() {
            let (_, at_query, at_ingestion) = expected
                .iter()
                .find(|(state, ..)| *state == properties)
                .expect("every valid state has an expectation");
            assert_eq!(
                properties.readable_at(QueryTime),
                *at_query,
                "{properties:?}"
            );
            assert_eq!(
                properties.readable_at(IngestionTime),
                *at_ingestion,
                "{properties:?}"
            );
        }
    }
}
