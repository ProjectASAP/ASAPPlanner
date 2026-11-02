//! Planner-level summary-window tumbling window primitives.
//!
//! These values describe tumbling-window layout and window-edge coverage. They do not
//! identify a runtime library, process, placement, shard layout, storage
//! backend, or deployment instance; those choices belong to downstream
//! physical compilation.

use asap_types::workload::RepeatedDemand;
use serde::{Deserialize, Serialize};

/// Concrete tumbling-window phase recorded in a catalog layout or inventory snapshot.
/// Milliseconds are canonical throughout the shared contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TumblingWindowLayout {
    // The deployed wire contract keeps the names from before #1163.
    #[serde(rename = "pane_width_ms", alias = "tumbling_window_width_ms")]
    pub tumbling_window_width_ms: u64,
    /// Unix timestamp of a tumbling-window edge. `None` means the runtime has not
    /// established an origin and therefore cannot claim full coverage.
    #[serde(rename = "pane_origin_ms")]
    pub tumbling_window_origin_ms: Option<i64>,
}

/// How a query obtains exact values for partial tumbling windows at its two window edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowEdgeCoverage {
    // Wire name kept from before #1163.
    #[serde(rename = "pane_aligned")]
    TumblingWindowAligned,
    #[serde(rename = "exact_boundary_residual")]
    ExactWindowEdgeResidual { executor: String, source: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// Wire names kept from before #1163.
pub enum TumblingWindowCoverageError {
    #[serde(rename = "ZeroPaneWidth")]
    ZeroTumblingWindowWidth,
    #[serde(rename = "UnknownPaneOrigin")]
    UnknownTumblingWindowOrigin,
    UnknownEvaluationPhase,
    PhaseMismatch {
        #[serde(rename = "pane_phase_ms")]
        tumbling_window_phase_ms: u64,
        query_phase_ms: u64,
    },
}

/// Validate that a tumbling-window-only evaluation covers a query exactly. A mismatched
/// phase is sound only when the physical plan explicitly supplies an exact
/// residual for the partial edge tumbling windows.
pub fn validate_tumbling_window_coverage(
    layout: &TumblingWindowLayout,
    evaluation_time_ms: Option<i64>,
    edge_coverage: &WindowEdgeCoverage,
) -> Result<(), TumblingWindowCoverageError> {
    if layout.tumbling_window_width_ms == 0 {
        return Err(TumblingWindowCoverageError::ZeroTumblingWindowWidth);
    }
    if matches!(
        edge_coverage,
        WindowEdgeCoverage::ExactWindowEdgeResidual { .. }
    ) {
        return Ok(());
    }
    let origin = layout
        .tumbling_window_origin_ms
        .ok_or(TumblingWindowCoverageError::UnknownTumblingWindowOrigin)?;
    let evaluation =
        evaluation_time_ms.ok_or(TumblingWindowCoverageError::UnknownEvaluationPhase)?;
    let width = layout.tumbling_window_width_ms as i64;
    let tumbling_window_phase_ms = origin.rem_euclid(width) as u64;
    let query_phase_ms = evaluation.rem_euclid(width) as u64;
    if tumbling_window_phase_ms == query_phase_ms {
        Ok(())
    } else {
        Err(TumblingWindowCoverageError::PhaseMismatch {
            tumbling_window_phase_ms,
            query_phase_ms,
        })
    }
}

/// Select a query-aligned tumbling-window origin from workload recurrence. A plain fixed
/// interval has cadence but no phase, so it remains unknown. Every timestamp
/// in an explicit schedule must have the same phase for one tumbling-window layout to
/// serve all occurrences.
pub(crate) fn plan_tumbling_window_phase(
    demand: &RepeatedDemand,
    tumbling_window_width_ms: u64,
) -> Result<TumblingWindowLayout, TumblingWindowCoverageError> {
    if tumbling_window_width_ms == 0 {
        return Err(TumblingWindowCoverageError::ZeroTumblingWindowWidth);
    }
    let phase = match demand {
        RepeatedDemand::FixedIntervalAt {
            evaluation_phase, ..
        } => Some(evaluation_phase.0),
        RepeatedDemand::Scheduled(times) => {
            let first = times.first().map(|t| t.0 % tumbling_window_width_ms);
            first.filter(|phase| {
                times
                    .iter()
                    .all(|t| t.0 % tumbling_window_width_ms == *phase)
            })
        }
        RepeatedDemand::FixedInterval(_) | RepeatedDemand::EstimatedRate(_) => None,
    };
    Ok(TumblingWindowLayout {
        tumbling_window_width_ms,
        tumbling_window_origin_ms: phase.and_then(|value| i64::try_from(value).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tumbling_window_only_evaluation_rejects_source_and_query_phase_mismatch() {
        let layout = TumblingWindowLayout {
            tumbling_window_width_ms: 60_000,
            tumbling_window_origin_ms: Some(26_000),
        };
        assert_eq!(
            validate_tumbling_window_coverage(
                &layout,
                Some(56_000),
                &WindowEdgeCoverage::TumblingWindowAligned
            ),
            Err(TumblingWindowCoverageError::PhaseMismatch {
                tumbling_window_phase_ms: 26_000,
                query_phase_ms: 56_000,
            })
        );
        assert!(validate_tumbling_window_coverage(
            &layout,
            Some(56_000),
            &WindowEdgeCoverage::ExactWindowEdgeResidual {
                executor: "prometheus".into(),
                source: "m".into(),
            },
        )
        .is_ok());
    }

    #[test]
    fn repeated_dashboard_phase_selects_query_aligned_origin() {
        let demand = RepeatedDemand::FixedIntervalAt {
            interval: asap_types::workload::RepetitionInterval(60_000),
            evaluation_phase: asap_types::workload::TimestampMs(56_000),
        };
        assert_eq!(
            plan_tumbling_window_phase(&demand, 60_000).unwrap(),
            TumblingWindowLayout {
                tumbling_window_width_ms: 60_000,
                tumbling_window_origin_ms: Some(56_000),
            }
        );
        assert_eq!(
            plan_tumbling_window_phase(
                &RepeatedDemand::FixedInterval(asap_types::workload::RepetitionInterval(60_000)),
                60_000,
            )
            .unwrap()
            .tumbling_window_origin_ms,
            None
        );
    }

    // Renamed tumbling window APIs still read and emit the deployed wire contract.
    #[test]
    fn window_edge_names_preserve_wire_values() {
        let layout: TumblingWindowLayout = serde_json::from_value(serde_json::json!({
            "pane_width_ms": 1000, "pane_origin_ms": 0
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&layout).unwrap(),
            serde_json::json!({"pane_width_ms": 1000, "pane_origin_ms": 0})
        );
        assert_eq!(
            serde_json::to_value(WindowEdgeCoverage::TumblingWindowAligned).unwrap(),
            serde_json::json!({"kind": "pane_aligned"})
        );
        let json = serde_json::json!({
            "kind": "exact_boundary_residual", "executor": "runtime", "source": "raw"
        });
        let coverage: WindowEdgeCoverage = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(
            coverage,
            WindowEdgeCoverage::ExactWindowEdgeResidual { .. }
        ));
        assert_eq!(serde_json::to_value(&coverage).unwrap(), json);
        assert!(validate_tumbling_window_coverage(&layout, Some(1500), &coverage).is_ok());
    }
}
