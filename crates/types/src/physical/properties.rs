//! A physical candidate's physical properties (#1200): when it runs
//! ([`ExecutionTiming`]) and how long its output lives ([`Lifetime`]).
//!
//! | timing | lifetime | how it runs |
//! |---|---|---|
//! | query | `None` | in each evaluation that reads it; discarded after its consumer runs |
//! | query | `OneEvaluation` | once per evaluation; its consumers in that evaluation share it |
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
    /// Discarded after its consumer's execution completes.
    #[default]
    None,
    /// Materialized until every consumer within one evaluation finishes.
    OneEvaluation(Medium),
    /// Materialized across evaluations, so later evaluations reuse it.
    AcrossEvaluations(Medium),
}

/// A physical property or physical requirement whose dimensions cannot go
/// together: bad input to [`PhysicalProperties::new`] or
/// [`PhysicalRequirement::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPhysicalProperties {
    #[error("an ingestion-time output cannot be materialized for one evaluation: there is no evaluation at ingestion time")]
    OneEvaluationAtIngestion,
    #[error("an ingestion-time consumer cannot read a later evaluation's output: there is no evaluation at ingestion time")]
    LaterEvaluationAtIngestion,
}

/// When a candidate runs and how long its output lives: one of the valid
/// rows of the table in this module's documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    pub fn timing(self) -> ExecutionTiming {
        self.timing
    }

    pub fn lifetime(self) -> Lifetime {
        self.lifetime
    }

    /// Whether the output outlives its consumer (lifetime `OneEvaluation` or
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
}

/// Which evaluation's output a consumer reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Evaluation {
    /// The output of the evaluation the consumer runs in (or, at ingestion
    /// time, of the ingestion work it runs in).
    Same,
    /// The output of an earlier evaluation, reused by a later one.
    Later,
}

/// The evaluation schedule (I_κ, φ_κ) of a consumer's execution context,
/// taken from its query's 𝒯_q: it evaluates at φ_κ + I_κ·ℤ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvaluationSchedule {
    /// I_κ, in milliseconds.
    pub interval_ms: u64,
    /// φ_κ, in milliseconds.
    pub phase_ms: u64,
}

/// r^phys_{c,i}: when a consumer reads one input, from which evaluation, and
/// on which schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalRequirement {
    consumer_timing: ExecutionTiming,
    reads_from: Evaluation,
    /// `None` for a consumer whose query is not periodic. No caller states one until the
    /// producers' schedule σ exists (#1200 PR 2).
    consumer_schedule: Option<EvaluationSchedule>,
}

impl PhysicalRequirement {
    /// The requirement of a consumer at `consumer_timing` reading `reads_from`
    /// on `consumer_schedule`, unless they cannot go together: an
    /// ingestion-time consumer cannot read a later evaluation.
    pub fn new(
        consumer_timing: ExecutionTiming,
        reads_from: Evaluation,
        consumer_schedule: Option<EvaluationSchedule>,
    ) -> Result<Self, InvalidPhysicalProperties> {
        use ExecutionTiming::{IngestionTime, QueryTime};
        match (consumer_timing, reads_from) {
            (IngestionTime, Evaluation::Later) => {
                Err(InvalidPhysicalProperties::LaterEvaluationAtIngestion)
            }
            (QueryTime | IngestionTime, Evaluation::Same) | (QueryTime, Evaluation::Later) => {
                Ok(Self {
                    consumer_timing,
                    reads_from,
                    consumer_schedule,
                })
            }
        }
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
    // #1200's schedule table needs the producer's σ (#1200 PR 2); until then
    // no consumer states a schedule.
    debug_assert!(requirement.consumer_schedule.is_none());
    match (
        producer.timing,
        producer.lifetime,
        requirement.consumer_timing,
        requirement.reads_from,
    ) {
        (IngestionTime, Lifetime::OneEvaluation(_), ..) => {
            unreachable!("rejected by PhysicalProperties::new")
        }
        (_, _, IngestionTime, Evaluation::Later) => {
            unreachable!("rejected by PhysicalRequirement::new")
        }
        // An ingestion-time consumer reads what runs at ingestion time.
        (
            IngestionTime,
            Lifetime::None | Lifetime::AcrossEvaluations(_),
            IngestionTime,
            Evaluation::Same,
        ) => true,
        (QueryTime, _, IngestionTime, Evaluation::Same) => false,
        // A query-time consumer in the same evaluation.
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
    use ExecutionTiming::{IngestionTime, QueryTime};

    /// Every timing x lifetime pair `new` accepts.
    fn valid_pairs() -> Vec<PhysicalProperties> {
        [QueryTime, IngestionTime]
            .into_iter()
            .flat_map(|timing| {
                [
                    Lifetime::None,
                    Lifetime::OneEvaluation(Medium::Memory),
                    Lifetime::OneEvaluation(Medium::Disk),
                    Lifetime::AcrossEvaluations(Medium::Memory),
                    Lifetime::AcrossEvaluations(Medium::Disk),
                ]
                .map(|lifetime| PhysicalProperties::new(timing, lifetime))
            })
            .filter_map(Result::ok)
            .collect()
    }

    /// Of the 10 timing x lifetime pairs, exactly the 2 at ingestion time for
    /// one evaluation are invalid.
    #[test]
    fn exactly_the_ingestion_time_one_evaluation_pairs_are_invalid() {
        for timing in [QueryTime, IngestionTime] {
            for medium in [Medium::Memory, Medium::Disk] {
                let one_evaluation =
                    PhysicalProperties::new(timing, Lifetime::OneEvaluation(medium));
                match timing {
                    IngestionTime => assert_eq!(
                        one_evaluation,
                        Err(InvalidPhysicalProperties::OneEvaluationAtIngestion)
                    ),
                    QueryTime => assert!(one_evaluation.is_ok()),
                }
            }
        }
        assert_eq!(valid_pairs().len(), 8);
    }

    /// `satisfies_physical` for every valid state and every valid requirement.
    #[test]
    fn satisfies_physical_over_every_valid_state() {
        let at = |timing, lifetime| PhysicalProperties::new(timing, lifetime).unwrap();
        let requirement =
            |timing, evaluation| PhysicalRequirement::new(timing, evaluation, None).unwrap();
        let same_at_query = requirement(QueryTime, Evaluation::Same);
        let same_at_ingestion = requirement(IngestionTime, Evaluation::Same);
        let later_at_query = requirement(QueryTime, Evaluation::Later);
        // (state, query time same evaluation, ingestion time, later evaluation)
        let mut expected = vec![
            (at(QueryTime, Lifetime::None), true, false, false),
            (at(IngestionTime, Lifetime::None), false, true, false),
        ];
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
        assert_eq!(expected.len(), valid_pairs().len());
        for (properties, query, ingestion, later) in expected {
            assert_eq!(
                satisfies_physical(&properties, &same_at_query),
                query,
                "{properties:?}"
            );
            assert_eq!(
                satisfies_physical(&properties, &same_at_ingestion),
                ingestion,
                "{properties:?}"
            );
            assert_eq!(
                satisfies_physical(&properties, &later_at_query),
                later,
                "{properties:?}"
            );
        }
    }

    /// `is_materialized` is lifetime ≠ None for every valid state.
    #[test]
    fn is_materialized_over_every_valid_state() {
        for properties in valid_pairs() {
            assert_eq!(
                properties.is_materialized(),
                properties.lifetime() != Lifetime::None,
                "{properties:?}"
            );
        }
    }

    /// An ingestion-time consumer cannot read a later evaluation; every other
    /// requirement is valid.
    #[test]
    fn later_evaluation_at_ingestion_is_the_invalid_requirement() {
        assert_eq!(
            PhysicalRequirement::new(IngestionTime, Evaluation::Later, None),
            Err(InvalidPhysicalProperties::LaterEvaluationAtIngestion)
        );
        for (timing, evaluation) in [
            (QueryTime, Evaluation::Same),
            (QueryTime, Evaluation::Later),
            (IngestionTime, Evaluation::Same),
        ] {
            assert!(PhysicalRequirement::new(timing, evaluation, None).is_ok());
        }
    }
}
