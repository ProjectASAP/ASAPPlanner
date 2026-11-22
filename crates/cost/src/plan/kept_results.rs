//! What a data plane keeps of its query results between requests
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 2): the outputs of query-time nodes computed once per evaluation time
//! and shared by the queries at that time, kept for a while after it, and
//! each query's final answer per evaluation time, kept so a range request
//! computes only the steps it has not answered before.

use asap_types::workload::{QueryWorkloadEntry, RangeEvaluation};

/// Evaluation times on a grid of `period_ms` within `kept_ms` behind the
/// newest (both ends included): the results kept of one node or query.
pub(crate) fn kept_per_grid(kept_ms: u64, period_ms: u64) -> u64 {
    kept_ms / period_ms.max(1) + 1
}

/// Steps one request of a query every `interval_ms` evaluating `range`
/// computes when the answers of its earlier steps are kept: those not
/// answered by the request before it, `interval / step`, at least 1 and at
/// most its steps. Without a range, 1.
pub(crate) fn steps_computed_per_request(
    range: Option<RangeEvaluation>,
    interval_ms: Option<u64>,
) -> u64 {
    let Some(range) = range else {
        return 1;
    };
    let steps = range.evaluations_per_request();
    match interval_ms {
        Some(interval_ms) => (interval_ms / range.step_ms.max(1)).clamp(1, steps),
        None => steps,
    }
}

/// `entry` as a data plane keeping its answers computes it: a range
/// request evaluates only its [`steps_computed_per_request`] newest steps,
/// as a range of that many. Its other fields are unchanged.
pub(crate) fn as_computed(entry: &QueryWorkloadEntry) -> QueryWorkloadEntry {
    let mut computed = entry.clone();
    if let Some(range) = entry.range_evaluation {
        let steps = steps_computed_per_request(Some(range), entry.recurrence.fixed_interval_ms());
        computed.range_evaluation = Some(RangeEvaluation {
            step_ms: range.step_ms,
            span_ms: (steps - 1) * range.step_ms,
        });
    }
    computed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query every 15 s whose answers are kept 1 h 1 min behind the newest
    /// keeps 245 of them; a node every 10 s kept 60 s keeps 7.
    #[test]
    fn kept_results_count_every_evaluation_time_within_the_horizon() {
        assert_eq!(kept_per_grid(3_660_000, 15_000), 245);
        assert_eq!(kept_per_grid(60_000, 10_000), 7);
    }

    /// A 1 h range at 15 s steps (241 steps) requested every 15 s computes
    /// 1 new step; every 5 min, 20; every 2 h, all 241; an instant query, 1.
    #[test]
    fn a_range_request_computes_only_the_steps_not_answered_before() {
        let range = Some(RangeEvaluation {
            step_ms: 15_000,
            span_ms: 3_600_000,
        });
        assert_eq!(steps_computed_per_request(range, Some(15_000)), 1);
        assert_eq!(steps_computed_per_request(range, Some(300_000)), 20);
        assert_eq!(steps_computed_per_request(range, Some(7_200_000)), 241);
        assert_eq!(steps_computed_per_request(None, Some(15_000)), 1);
        assert_eq!(steps_computed_per_request(range, None), 241);
    }
}
