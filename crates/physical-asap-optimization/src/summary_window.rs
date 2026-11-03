//! The tumbling-window phase a workload's recurrence implies.

use asap_types::workload::RepeatedDemand;

/// The phase (modulo `tumbling_window_width_ms`, nonzero) of every evaluation
/// of `demand`, if known. A plain fixed interval has cadence but no phase, so
/// it is unknown. An explicit schedule has one only when every timestamp in
/// it has the same phase.
pub(crate) fn plan_tumbling_window_phase(
    demand: &RepeatedDemand,
    tumbling_window_width_ms: u64,
) -> Option<u64> {
    match demand {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A known evaluation phase is the phase; a bare interval has none.
    #[test]
    fn repeated_dashboard_phase_selects_query_aligned_origin() {
        let demand = RepeatedDemand::FixedIntervalAt {
            interval: asap_types::workload::RepetitionInterval(60_000),
            evaluation_phase: asap_types::workload::TimestampMs(56_000),
        };
        assert_eq!(plan_tumbling_window_phase(&demand, 60_000), Some(56_000));
        assert_eq!(
            plan_tumbling_window_phase(
                &RepeatedDemand::FixedInterval(asap_types::workload::RepetitionInterval(60_000)),
                60_000,
            ),
            None
        );
    }
}
