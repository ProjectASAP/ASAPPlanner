//! What a data plane keeps of its query results between requests
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 2): the outputs of query-time nodes computed once per evaluation time
//! and shared by the queries at that time, kept for a while after it, and
//! each query's final answer per evaluation time, kept so a range request
//! computes only the steps it has not answered before.

use crate::AnswerCache;
use asap_types::workload::{QueryWorkloadEntry, RangeEvaluation};

/// Evaluation times on a grid of `period_ms` within `kept_ms` behind the
/// newest (both ends included), on average over where the newest falls:
/// the results kept of one node or query, `kept / period + 1`. Linear in
/// `kept_ms`, so Stage 3's selection can charge it over a retention it
/// chooses.
pub fn kept_per_grid(kept_ms: u64, period_ms: u64) -> f64 {
    kept_ms as f64 / period_ms.max(1) as f64 + 1.0
}

/// A pool of kept bytes, `total`, under a cache bound `max_bytes`: what it
/// holds, at most the bound (a bounded cache drops what it cannot hold).
pub fn held_by_bounded_cache(total: f64, max_bytes: Option<u64>) -> f64 {
    match max_bytes {
        Some(max_bytes) => total.min(max_bytes as f64),
        None => total,
    }
}

/// Steps one request of a query every `interval_ms` evaluating `range`
/// computes when the answers of its earlier steps are kept, on average:
/// those not answered by the request before it, `interval / step` in
/// reals, at least 1 and at most its steps. Without a range, 1.
///
/// Example: a 1 h range at 15 s steps every 20 s computes 1, 2, then 1
/// new steps: 4/3.
pub fn steps_computed_per_request(range: Option<RangeEvaluation>, interval_ms: Option<u64>) -> f64 {
    let Some(range) = range else {
        return 1.0;
    };
    let steps = range.evaluations_per_request() as f64;
    match interval_ms {
        Some(interval_ms) => (interval_ms as f64 / range.step_ms.max(1) as f64).clamp(1.0, steps),
        None => steps,
    }
}

/// The range one request of `entry` evaluates, as the data plane computes
/// it: its range evaluation; with an answer cache, only its
/// [`steps_computed_per_request`] new steps on average, a range whose span
/// is that many steps less one (`RangeEvaluation::mean_evaluations_per_request`;
/// not a multiple of the step when the mean is fractional). `None` for an
/// instant query. Only the steps the cost model counts per request take it
/// (rates, per-step fees, latency rounds, window reads): the entry itself,
/// its evaluation grid, request times and retention, is the user's.
///
/// Example: a 1 h range at 15 s steps every 20 s, with an answer cache: a
/// 5 s span at 15 s steps, 4/3 steps per request.
pub fn computed_range(
    entry: &QueryWorkloadEntry,
    answer_cache: Option<AnswerCache>,
) -> Option<RangeEvaluation> {
    let range = entry.range_evaluation?;
    if answer_cache.is_none() {
        return Some(range);
    }
    let steps = steps_computed_per_request(Some(range), entry.recurrence.fixed_interval_ms());
    Some(RangeEvaluation {
        step_ms: range.step_ms,
        span_ms: ((steps - 1.0) * range.step_ms as f64).round() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A query every 15 s whose answers are kept 1 h 1 min behind the newest
    /// keeps 245 of them; a node every 10 s kept 60 s keeps 7, kept 65 s 7.5
    /// on average.
    #[test]
    fn kept_results_count_every_evaluation_time_within_the_horizon() {
        assert_eq!(kept_per_grid(3_660_000, 15_000), 245.0);
        assert_eq!(kept_per_grid(60_000, 10_000), 7.0);
        assert_eq!(kept_per_grid(65_000, 10_000), 7.5);
    }

    /// A 1 h range at 15 s steps (241 steps) requested every 15 s computes
    /// 1 new step; every 5 min, 20; every 2 h, all 241; an instant query, 1.
    #[test]
    fn a_range_request_computes_only_the_steps_not_answered_before() {
        let range = Some(RangeEvaluation {
            step_ms: 15_000,
            span_ms: 3_600_000,
        });
        assert_eq!(steps_computed_per_request(range, Some(15_000)), 1.0);
        assert_eq!(steps_computed_per_request(range, Some(300_000)), 20.0);
        assert_eq!(steps_computed_per_request(range, Some(7_200_000)), 241.0);
        assert_eq!(steps_computed_per_request(None, Some(15_000)), 1.0);
        assert_eq!(steps_computed_per_request(range, None), 241.0);
    }

    /// A request at `t` evaluates its steps back from its end, `t − j · step`,
    /// and an answer is kept by its evaluation time, so it reuses only the
    /// steps of the request a whole number of steps earlier: the one
    /// `lcm(interval, step)` before. Every 20 s at 15 s steps that is the
    /// request 60 s before: 4 new steps per request (not 4/3); every 10 s,
    /// the one 30 s before: 2 (not 2/3, not 1); every 2 h, all 241.
    #[test]
    fn a_range_request_reuses_the_steps_of_the_request_a_whole_number_of_steps_before() {
        let range = RangeEvaluation {
            step_ms: 15_000,
            span_ms: 3_600_000,
        };
        assert_eq!(steps_computed_per_request(Some(range), Some(20_000)), 4.0);
        assert_eq!(steps_computed_per_request(Some(range), Some(10_000)), 2.0);
        assert_eq!(steps_computed_per_request(Some(range), Some(7_200_000)), 241.0);
        let workload: asap_types::workload::PlanningWorkload =
            serde_json::from_value(serde_json::json!({
                "query_workload": {"language": "promql", "repeating_queries": [{
                    "query": "max_over_time(m[5m])",
                    "demand": {"fixed_interval_at": {"interval": 20000, "evaluation_phase": 0}},
                    "requirements": {"max_latency_ms": null},
                    "predictability": {"predictable": {"known_at": null}},
                    "range_evaluation": {"step_ms": 15000, "span_ms": 3600000},
                }]},
            }))
            .unwrap();
        let entry = workload.query_workload.entries().next().unwrap();
        let answers = Some(AnswerCache {
            retention_margin_ms: 60_000,
            max_bytes: None,
        });
        let computed = computed_range(&entry, answers).expect("a range");
        assert_eq!(computed.mean_evaluations_per_request(), 4.0);
        assert_eq!(computed_range(&entry, None), entry.range_evaluation);
    }
}
