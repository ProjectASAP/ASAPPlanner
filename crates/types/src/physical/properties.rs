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

    /// Whether the output outlives its reader (lifetime `OneEvaluation` or
    /// `AcrossEvaluations`): materialized(v) ⇔ ℓ_v ≠ None.
    pub fn is_materialized(self) -> bool {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (self.timing, self.lifetime) {
            (QueryTime | IngestionTime, Lifetime::None) => false,
            (QueryTime, Lifetime::OneEvaluation(_) | Lifetime::AcrossEvaluations(_))
            | (IngestionTime, Lifetime::AcrossEvaluations(_)) => true,
            (IngestionTime, Lifetime::OneEvaluation(_)) => {
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

/// Which evaluation's output a reader reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Evaluation {
    /// The output of the evaluation the reader runs in (or, at ingestion
    /// time, of the ingestion work it runs in).
    Same,
    /// The output of an earlier evaluation, reused by a later one.
    Later,
}

/// A reader timing and an evaluation that cannot go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPhysicalRequirement {
    #[error("an ingestion-time reader cannot read a later evaluation's output: there is no evaluation at ingestion time")]
    LaterEvaluationAtIngestion,
}

/// r^phys_{c,i}: when a consumer reads one input, and from which
/// evaluation. The reader's schedule comes with the schedule σ (#1200 PR 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalRequirement {
    reader_timing: ExecutionTiming,
    reads_from: Evaluation,
}

impl PhysicalRequirement {
    /// The requirement of a reader at `reader_timing` reading `reads_from`,
    /// unless they cannot go together.
    pub fn new(
        reader_timing: ExecutionTiming,
        reads_from: Evaluation,
    ) -> Result<Self, InvalidPhysicalRequirement> {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (reader_timing, reads_from) {
            (IngestionTime, Evaluation::Later) => {
                Err(InvalidPhysicalRequirement::LaterEvaluationAtIngestion)
            }
            (QueryTime | IngestionTime, Evaluation::Same) | (QueryTime, Evaluation::Later) => {
                Ok(Self {
                    reader_timing,
                    reads_from,
                })
            }
        }
    }

    pub fn reader_timing(self) -> ExecutionTiming {
        self.reader_timing
    }

    pub fn reads_from(self) -> Evaluation {
        self.reads_from
    }
}

/// α^phys_{u,c,i}: whether a producer with `producer`'s properties may serve
/// an input with `requirement` (#1200, timing and lifetime table):
/// - at ingestion time: the producer runs at ingestion time;
/// - at query time, in the same evaluation: a query-time producer with any
///   lifetime, or an ingestion-time producer materialized across evaluations;
/// - at query time, in a later evaluation: a producer materialized across
///   evaluations.
pub fn satisfies_physical(
    producer: &PhysicalProperties,
    requirement: &PhysicalRequirement,
) -> bool {
    use ExecutionTiming::{IngestionTime, QueryTime};
    match (
        producer.timing,
        producer.lifetime,
        requirement.reader_timing,
        requirement.reads_from,
    ) {
        (IngestionTime, Lifetime::OneEvaluation(_), ..) => {
            unreachable!("rejected by PhysicalProperties::new")
        }
        (_, _, IngestionTime, Evaluation::Later) => {
            unreachable!("rejected by PhysicalRequirement::new")
        }
        // An ingestion-time reader reads what runs at ingestion time.
        (
            IngestionTime,
            Lifetime::None | Lifetime::AcrossEvaluations(_),
            IngestionTime,
            Evaluation::Same,
        ) => true,
        (QueryTime, _, IngestionTime, Evaluation::Same) => false,
        // A query-time reader in the same evaluation.
        (QueryTime, _, QueryTime, Evaluation::Same) => true,
        (IngestionTime, Lifetime::AcrossEvaluations(_), QueryTime, Evaluation::Same) => true,
        (IngestionTime, Lifetime::None, QueryTime, Evaluation::Same) => false,
        // A later evaluation reads only what lives across evaluations.
        (_, Lifetime::AcrossEvaluations(_), QueryTime, Evaluation::Later) => true,
        (_, Lifetime::None | Lifetime::OneEvaluation(_), QueryTime, Evaluation::Later) => false,
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

    /// `satisfies_physical` for every valid state and every valid requirement.
    #[test]
    fn satisfies_physical_over_every_valid_state() {
        use ExecutionTiming::{IngestionTime, QueryTime};
        let at = |timing, lifetime| PhysicalProperties::new(timing, lifetime).unwrap();
        let same_at_query = PhysicalRequirement::new(QueryTime, Evaluation::Same).unwrap();
        let same_at_ingestion = PhysicalRequirement::new(IngestionTime, Evaluation::Same).unwrap();
        let later_at_query = PhysicalRequirement::new(QueryTime, Evaluation::Later).unwrap();
        // (state, query time same evaluation, ingestion time, later evaluation)
        let mut expected = Vec::new();
        for medium in [Medium::Memory, Medium::Disk] {
            expected.extend([
                (
                    at(QueryTime, Lifetime::OneEvaluation(medium)),
                    true,
                    false,
                    false,
                ),
                (
                    at(QueryTime, Lifetime::AcrossEvaluations(medium)),
                    true,
                    false,
                    true,
                ),
                (
                    at(IngestionTime, Lifetime::AcrossEvaluations(medium)),
                    true,
                    true,
                    true,
                ),
            ]);
        }
        expected.extend([
            (at(QueryTime, Lifetime::None), true, false, false),
            (at(IngestionTime, Lifetime::None), false, true, false),
        ]);
        assert_eq!(expected.len(), PhysicalProperties::every_valid().len());
        for properties in PhysicalProperties::every_valid() {
            let (_, query, ingestion, later) = expected
                .iter()
                .find(|(state, ..)| *state == properties)
                .expect("every valid state has an expectation");
            assert_eq!(
                satisfies_physical(&properties, &same_at_query),
                *query,
                "{properties:?}"
            );
            assert_eq!(
                satisfies_physical(&properties, &same_at_ingestion),
                *ingestion,
                "{properties:?}"
            );
            assert_eq!(
                satisfies_physical(&properties, &later_at_query),
                *later,
                "{properties:?}"
            );
        }
    }

    /// `is_materialized` is lifetime ≠ None for every valid state.
    #[test]
    fn is_materialized_over_every_valid_state() {
        for properties in PhysicalProperties::every_valid() {
            assert_eq!(
                properties.is_materialized(),
                properties.lifetime() != Lifetime::None,
                "{properties:?}"
            );
        }
    }

    /// An ingestion-time reader cannot read a later evaluation; every other
    /// requirement is valid.
    #[test]
    fn later_evaluation_at_ingestion_is_the_invalid_requirement() {
        use ExecutionTiming::{IngestionTime, QueryTime};
        assert_eq!(
            PhysicalRequirement::new(IngestionTime, Evaluation::Later),
            Err(InvalidPhysicalRequirement::LaterEvaluationAtIngestion)
        );
        for (timing, evaluation) in [
            (QueryTime, Evaluation::Same),
            (QueryTime, Evaluation::Later),
            (IngestionTime, Evaluation::Same),
        ] {
            let requirement = PhysicalRequirement::new(timing, evaluation).unwrap();
            assert_eq!(
                (requirement.reader_timing(), requirement.reads_from()),
                (timing, evaluation)
            );
        }
    }
}
