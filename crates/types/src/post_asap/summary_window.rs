//! Planner-level summary-window primitives.
//!
//! These values identify the abstract window framework selected during
//! candidate search. They do not identify a runtime library, process,
//! placement, shard layout, storage backend, or deployment instance; those
//! choices belong to downstream physical compilation.

use crate::workload::RepeatedDemand;
use serde::{Deserialize, Serialize};

/// Abstract framework used to organize incrementally maintained summary
/// state over time.
///
/// The built-in variants name semantics that the planner can compare across
/// implementations. [`Self::Extension`] lets a provider introduce a new
/// primitive without treating an opaque physical deployment ID as planner IR.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryWindowFramework {
    /// Disjoint, fixed-width windows.
    Tumbling,
    /// Overlapping logical windows, commonly realized from reusable panes.
    Sliding,
    /// Hierarchical buckets with exponentially increasing coverage.
    ExponentialHistogram,
    /// A named planner primitive whose semantics are registered by a provider.
    Extension(String),
}

/// Concrete pane phase recorded in a catalog binding or inventory snapshot.
/// Milliseconds are canonical throughout the shared contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePhaseBinding {
    pub pane_width_ms: u64,
    /// Unix timestamp of a pane boundary. `None` means the runtime has not
    /// established an origin and therefore cannot claim full coverage.
    pub pane_origin_ms: Option<i64>,
}

/// How a query obtains exact values for partial panes at its two boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BoundaryCoverage {
    PaneAligned,
    ExactBoundaryResidual { executor: String, source: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneCoverageError {
    ZeroPaneWidth,
    UnknownPaneOrigin,
    UnknownEvaluationPhase,
    PhaseMismatch {
        pane_phase_ms: u64,
        query_phase_ms: u64,
    },
}

/// Validate that a pane-only readout covers a query exactly. A mismatched
/// phase is sound only when the physical plan explicitly supplies an exact
/// residual for the partial boundary panes.
pub fn validate_pane_coverage(
    binding: &PanePhaseBinding,
    evaluation_time_ms: Option<i64>,
    boundary: &BoundaryCoverage,
) -> Result<(), PaneCoverageError> {
    if binding.pane_width_ms == 0 {
        return Err(PaneCoverageError::ZeroPaneWidth);
    }
    if matches!(boundary, BoundaryCoverage::ExactBoundaryResidual { .. }) {
        return Ok(());
    }
    let origin = binding
        .pane_origin_ms
        .ok_or(PaneCoverageError::UnknownPaneOrigin)?;
    let evaluation = evaluation_time_ms.ok_or(PaneCoverageError::UnknownEvaluationPhase)?;
    let width = binding.pane_width_ms as i64;
    let pane_phase_ms = origin.rem_euclid(width) as u64;
    let query_phase_ms = evaluation.rem_euclid(width) as u64;
    if pane_phase_ms == query_phase_ms {
        Ok(())
    } else {
        Err(PaneCoverageError::PhaseMismatch {
            pane_phase_ms,
            query_phase_ms,
        })
    }
}

/// Select a query-aligned pane origin from workload recurrence. A plain fixed
/// interval has cadence but no phase, so it remains unknown. Every timestamp
/// in an explicit schedule must have the same phase for one pane layout to
/// serve all occurrences.
pub fn plan_pane_phase(
    demand: &RepeatedDemand,
    pane_width_ms: u64,
) -> Result<PanePhaseBinding, PaneCoverageError> {
    if pane_width_ms == 0 {
        return Err(PaneCoverageError::ZeroPaneWidth);
    }
    let phase = match demand {
        RepeatedDemand::FixedIntervalAt {
            evaluation_phase, ..
        } => Some(evaluation_phase.0),
        RepeatedDemand::Scheduled(times) => {
            let first = times.first().map(|t| t.0 % pane_width_ms);
            first.filter(|phase| times.iter().all(|t| t.0 % pane_width_ms == *phase))
        }
        RepeatedDemand::FixedInterval(_) | RepeatedDemand::EstimatedRate(_) => None,
    };
    Ok(PanePhaseBinding {
        pane_width_ms,
        pane_origin_ms: phase.and_then(|value| i64::try_from(value).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_extension_frameworks_round_trip() {
        for framework in [
            SummaryWindowFramework::Tumbling,
            SummaryWindowFramework::Sliding,
            SummaryWindowFramework::ExponentialHistogram,
            SummaryWindowFramework::Extension("learned_window".into()),
        ] {
            let encoded = serde_json::to_string(&framework).unwrap();
            assert_eq!(
                serde_json::from_str::<SummaryWindowFramework>(&encoded).unwrap(),
                framework
            );
        }
    }

    #[test]
    fn pane_only_readout_rejects_source_and_query_phase_mismatch() {
        let binding = PanePhaseBinding {
            pane_width_ms: 60_000,
            pane_origin_ms: Some(26_000),
        };
        assert_eq!(
            validate_pane_coverage(&binding, Some(56_000), &BoundaryCoverage::PaneAligned),
            Err(PaneCoverageError::PhaseMismatch {
                pane_phase_ms: 26_000,
                query_phase_ms: 56_000,
            })
        );
        assert!(validate_pane_coverage(
            &binding,
            Some(56_000),
            &BoundaryCoverage::ExactBoundaryResidual {
                executor: "prometheus".into(),
                source: "m".into(),
            },
        )
        .is_ok());
    }

    #[test]
    fn repeated_dashboard_phase_selects_query_aligned_origin() {
        let demand = RepeatedDemand::FixedIntervalAt {
            interval: crate::workload::RepetitionInterval(60_000),
            evaluation_phase: crate::workload::TimestampMs(56_000),
        };
        assert_eq!(
            plan_pane_phase(&demand, 60_000).unwrap(),
            PanePhaseBinding {
                pane_width_ms: 60_000,
                pane_origin_ms: Some(56_000),
            }
        );
        assert_eq!(
            plan_pane_phase(
                &RepeatedDemand::FixedInterval(crate::workload::RepetitionInterval(60_000)),
                60_000,
            )
            .unwrap()
            .pane_origin_ms,
            None
        );
    }
}
