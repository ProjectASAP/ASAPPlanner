//! A physical candidate's physical properties $p^{\mathrm{phys}}_v = (\tau, \ell)$
//! (#1200; concepts doc `memo-groups-properties-requirements.md`, sections 1
//! and 3): when it runs ([`ExecutionTiming`]) and how long its output lives
//! ([`Lifetime`]). There is no schedule property: a `None` or `OneEvaluation`
//! output runs on the schedule of the context κ that runs it, and a tumbling
//! grid is anchored at phase 0 with its width in the logical window layout.
//!
//! | timing | lifetime | runs |
//! |---|---|---|
//! | query | `None` | in each context κ that reads it |
//! | query | `OneEvaluation` | in one context κ; that evaluation's consumers share it |
//! | query | `AcrossEvaluations` | one window when each window ends |
//! | ingestion | `None` | inside the ingestion work that reads it |
//! | ingestion | `AcrossEvaluations` | one window when each window ends |
//! | ingestion | `OneEvaluation` | invalid: there is no evaluation at ingestion time |
//!
//! There is no stored physical requirement: $r^{\mathrm{phys}}_{c,i}$ is a
//! function of the consumer's own properties
//! ([`crate::stages::producer_satisfies_physical_requirements`]).

pub use crate::ir::properties::ExecutionTiming;

/// Where a materialized output is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum StorageMedium {
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
    OneEvaluation(StorageMedium),
    /// Materialized across evaluations, so later evaluations reuse it.
    AcrossEvaluations(StorageMedium),
}

/// A timing and lifetime that cannot go together (the invalid combination
/// of the concepts doc, section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PhysicalPropertiesError {
    #[error("an ingestion-time output cannot be materialized for one evaluation: there is no evaluation at ingestion time")]
    OneEvaluationAtIngestion,
}

/// $p^{\mathrm{phys}}_v = (\tau, \ell)$. [`Self::new`] rejects the invalid
/// combination (concepts doc, section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PhysicalProperties {
    timing: ExecutionTiming,
    lifetime: Lifetime,
}

impl PhysicalProperties {
    /// The properties `(timing, lifetime)`, unless they cannot go together.
    pub fn new(
        timing: ExecutionTiming,
        lifetime: Lifetime,
    ) -> Result<Self, PhysicalPropertiesError> {
        match (timing, lifetime) {
            (ExecutionTiming::IngestionTime, Lifetime::OneEvaluation(_)) => {
                Err(PhysicalPropertiesError::OneEvaluationAtIngestion)
            }
            _ => Ok(Self { timing, lifetime }),
        }
    }

    /// $\tau_v$.
    pub fn timing(&self) -> ExecutionTiming {
        self.timing
    }

    /// $\ell_v$.
    pub fn lifetime(&self) -> Lifetime {
        self.lifetime
    }

    /// Whether $\ell_v \ne$ None. Derived, not stored.
    pub fn is_materialized(&self) -> bool {
        self.lifetime != Lifetime::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ExecutionTiming::{IngestionTime, QueryTime};

    /// `new` accepts every (τ, ℓ) except (IngestionTime, OneEvaluation), for
    /// both media, and keeps what it was given.
    #[test]
    fn new_over_every_timing_and_lifetime() {
        for medium in [StorageMedium::Memory, StorageMedium::Disk] {
            for (timing, lifetime, valid) in [
                (QueryTime, Lifetime::None, true),
                (QueryTime, Lifetime::OneEvaluation(medium), true),
                (QueryTime, Lifetime::AcrossEvaluations(medium), true),
                (IngestionTime, Lifetime::None, true),
                (IngestionTime, Lifetime::OneEvaluation(medium), false),
                (IngestionTime, Lifetime::AcrossEvaluations(medium), true),
            ] {
                let properties = PhysicalProperties::new(timing, lifetime);
                match valid {
                    true => {
                        let properties = properties.unwrap();
                        assert_eq!(
                            (properties.timing(), properties.lifetime()),
                            (timing, lifetime)
                        );
                        assert_eq!(properties.is_materialized(), lifetime != Lifetime::None);
                    }
                    false => assert_eq!(
                        properties,
                        Err(PhysicalPropertiesError::OneEvaluationAtIngestion)
                    ),
                }
            }
        }
    }
}
