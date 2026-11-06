//! A physical candidate's physical properties $p^{\mathrm{phys}}_v = (\tau, \ell, \sigma)$
//! (#1200; concepts doc `memo-groups-properties-requirements.md`, sections 1
//! and 3): when it runs ([`ExecutionTiming`]), how long its output lives
//! ([`Lifetime`]) and, for a materialized output, on which times or grid it
//! is delivered ([`Schedule`]).
//!
//! | timing | lifetime | schedule |
//! |---|---|---|
//! | query | `None` | none: runs in each context κ that reads it |
//! | query | `OneEvaluation` | `Periodic { interval, phase }` |
//! | query | `AcrossEvaluations` | `TumblingWindowGrid { phase }` |
//! | ingestion | `None` | none: runs inside the ingestion work that reads it |
//! | ingestion | `AcrossEvaluations` | `TumblingWindowGrid { phase: 0 }` |
//! | ingestion | `OneEvaluation` | invalid: there is no evaluation at ingestion time |
//!
//! There is no stored physical requirement: $r^{\mathrm{phys}}_{c,i}$ is a
//! function of the consumer's own properties
//! ([`crate::stages::satisfies_physical`]).

pub use crate::ir::properties::ExecutionTiming;

/// Where a materialized output is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Medium {
    Memory,
    Disk,
}

/// $\ell_v$: how long a candidate's output lives. Only a materialized output
/// has a medium.
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

/// $\sigma_v$: what a materialized output ($\ell \ne$ None) is delivered on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Schedule {
    /// `OneEvaluation`: delivered at $\varphi + I\cdot\mathbb Z$.
    Periodic { interval_ms: u64, phase_ms: u64 },
    /// `AcrossEvaluations`: the tumbling-window grid's phase, modulo the
    /// grid's width.
    TumblingWindowGrid { phase_ms: u64 },
}

/// A timing, lifetime and schedule that cannot go together (the invalid
/// combinations of the concepts doc, section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPhysicalProperties {
    #[error("an ingestion-time output cannot be materialized for one evaluation: there is no evaluation at ingestion time")]
    OneEvaluationAtIngestion,
    #[error("an output that is not materialized has no schedule: it runs in its consumer's execution context")]
    ScheduleWithoutMaterialization,
    #[error("a materialized output needs a schedule")]
    MaterializedWithoutSchedule,
    #[error(
        "one evaluation needs a periodic schedule; across evaluations needs a tumbling-window grid"
    )]
    ScheduleDoesNotMatchLifetime,
    #[error("a periodic schedule needs a positive interval and a phase below it")]
    PhaseNotBelowInterval,
    #[error("an ingestion-time tumbling-window grid is anchored at the epoch: its phase is 0")]
    IngestionGridPhaseNotZero,
}

/// $p^{\mathrm{phys}}_v = (\tau, \ell, \sigma)$. [`Self::new`] rejects the
/// invalid combinations (concepts doc, section 2). A grid schedule's
/// phase below the tumbling width is checked where the width is known: when
/// a `PhysicalCandidate` is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalProperties {
    timing: ExecutionTiming,
    lifetime: Lifetime,
    schedule: Option<Schedule>,
}

impl PhysicalProperties {
    /// The properties `(timing, lifetime, schedule)`, unless they cannot go
    /// together.
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
        schedule: Option<Schedule>,
    ) -> Result<Self, InvalidPhysicalProperties> {
        let _ = (timing, lifetime, schedule);
        todo!("#1207 PR 1: reject the invalid combinations of section 2")
    }

    /// $\tau_v$.
    pub fn timing(&self) -> ExecutionTiming {
        self.timing
    }

    /// $\ell_v$.
    pub fn lifetime(&self) -> Lifetime {
        self.lifetime
    }

    /// $\sigma_v$: `None` exactly when $\ell_v$ = None.
    pub fn schedule(&self) -> Option<&Schedule> {
        self.schedule.as_ref()
    }

    /// Whether $\ell_v \ne$ None. Derived, not stored.
    pub fn is_materialized(&self) -> bool {
        self.lifetime != Lifetime::None
    }
}

/// A periodic schedule $\varphi + I\cdot\mathbb Z$: a materialized consumer's
/// own times, or a context's times $T_\kappa$ (Stage 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvaluationSchedule {
    interval_ms: u64,
    phase_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ExecutionTiming::{IngestionTime, QueryTime};

    const MEMORY: Medium = Medium::Memory;

    fn periodic(interval_ms: u64, phase_ms: u64) -> Option<Schedule> {
        Some(Schedule::Periodic {
            interval_ms,
            phase_ms,
        })
    }

    fn grid(phase_ms: u64) -> Option<Schedule> {
        Some(Schedule::TumblingWindowGrid { phase_ms })
    }

    /// Every invalid combination of the concepts doc, section 2.
    #[test]
    fn new_rejects_each_invalid_combination() {
        use InvalidPhysicalProperties as E;
        let cases = [
            (
                IngestionTime,
                Lifetime::OneEvaluation(MEMORY),
                periodic(10_000, 0),
                E::OneEvaluationAtIngestion,
            ),
            (
                QueryTime,
                Lifetime::None,
                periodic(10_000, 0),
                E::ScheduleWithoutMaterialization,
            ),
            (
                QueryTime,
                Lifetime::OneEvaluation(MEMORY),
                None,
                E::MaterializedWithoutSchedule,
            ),
            (
                QueryTime,
                Lifetime::OneEvaluation(MEMORY),
                grid(0),
                E::ScheduleDoesNotMatchLifetime,
            ),
            (
                QueryTime,
                Lifetime::AcrossEvaluations(MEMORY),
                periodic(10_000, 0),
                E::ScheduleDoesNotMatchLifetime,
            ),
            (
                QueryTime,
                Lifetime::OneEvaluation(MEMORY),
                periodic(10_000, 10_000),
                E::PhaseNotBelowInterval,
            ),
            (
                QueryTime,
                Lifetime::OneEvaluation(MEMORY),
                periodic(0, 0),
                E::PhaseNotBelowInterval,
            ),
            (
                IngestionTime,
                Lifetime::AcrossEvaluations(MEMORY),
                grid(5_000),
                E::IngestionGridPhaseNotZero,
            ),
        ];
        for (timing, lifetime, schedule, error) in cases {
            assert_eq!(
                PhysicalProperties::new(timing, lifetime, schedule),
                Err(error)
            );
        }
    }

    /// The valid (τ, ℓ) pairs of section 2, each with a schedule of its kind.
    #[test]
    fn new_accepts_each_valid_pair() {
        for (timing, lifetime, schedule) in [
            (QueryTime, Lifetime::None, None),
            (IngestionTime, Lifetime::None, None),
            (
                QueryTime,
                Lifetime::OneEvaluation(MEMORY),
                periodic(10_000, 5_000),
            ),
            (QueryTime, Lifetime::AcrossEvaluations(MEMORY), grid(5_000)),
            (
                IngestionTime,
                Lifetime::AcrossEvaluations(Medium::Disk),
                grid(0),
            ),
        ] {
            let properties = PhysicalProperties::new(timing, lifetime, schedule).unwrap();
            assert_eq!(properties.is_materialized(), lifetime != Lifetime::None);
        }
    }
}
