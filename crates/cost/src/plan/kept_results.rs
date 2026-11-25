//! What a data plane keeps of its query results between requests
//! (`docs/design_docs/proposals/cost-terms-from-calibration.md`, section
//! 2): the outputs of query-time nodes computed once per evaluation time
//! and shared by the queries at that time, kept for a while after it, and
//! each query's final answer per evaluation time, kept so a range request
//! computes only the steps it has not answered before.

use crate::AnswerCache;
use asap_types::workload::{least_common_multiple, QueryWorkloadEntry, RangeEvaluation};

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

/// The answers an answer cache keeps ([`answers_kept`]): each query's
/// answer per evaluation time as far back as the plan's evaluations read
/// (its longest retention) plus the cache's margin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnswersKept {
    /// How far back answers are kept, in ms: the longest retention plus
    /// `AnswerCache::retention_margin_ms`.
    pub horizon_ms: u64,
    /// The bytes of every query's kept answers.
    pub total_bytes: f64,
    /// The bytes the cache holds of them: at most its bound
    /// ([`held_by_bounded_cache`]).
    pub held_bytes: f64,
}

impl AnswersKept {
    /// The bytes kept of one query whose answer is `answer_bytes`,
    /// evaluated every `period_ms`, before the cache's bound.
    pub fn bytes_of(&self, answer_bytes: f64, period_ms: u64) -> f64 {
        answer_bytes * kept_per_grid(self.horizon_ms, period_ms)
    }
}

/// What an answer cache keeps of the queries' answers, each given by its
/// answer's bytes and its evaluation period in ms, under a plan whose
/// longest retention is `longest_retention_ms` ([`AnswersKept`]).
///
/// Example: a 1 h retention, a 1 min margin, one query every 15 s whose
/// answer is 1,000 bytes: a 3,660,000 ms horizon, 245,000 bytes kept.
pub fn answers_kept(
    longest_retention_ms: u64,
    answers: AnswerCache,
    answer_bytes_and_period_ms: impl IntoIterator<Item = (f64, u64)>,
) -> AnswersKept {
    let mut kept = AnswersKept {
        horizon_ms: longest_retention_ms.saturating_add(answers.retention_margin_ms),
        total_bytes: 0.0,
        held_bytes: 0.0,
    };
    kept.total_bytes = answer_bytes_and_period_ms
        .into_iter()
        .map(|(bytes, period_ms)| kept.bytes_of(bytes, period_ms))
        .sum();
    kept.held_bytes = held_by_bounded_cache(kept.total_bytes, answers.max_bytes);
    kept
}

/// Steps one request of a query every `interval_ms` evaluating `range`
/// computes when the answers of its earlier steps are kept by evaluation
/// time. A request at `t` evaluates `t − j · step` (its steps run back from
/// its end), so it finds kept only the steps of a request a whole number of
/// steps earlier, the one `lcm(interval, step)` before: it computes
/// `lcm(interval, step) / step` new steps, at most its steps. Without a
/// range, 1; without a fixed interval, every step.
///
/// Examples: a 1 h range at 15 s steps (241 steps) every 15 s computes 1
/// step; every 20 s, 4 (the request 60 s before shares the rest); every
/// 10 s, 2; every 5 min, 20; every 2 h, all 241.
fn steps_computed_per_request(range: Option<RangeEvaluation>, interval_ms: Option<u64>) -> f64 {
    let Some(range) = range else {
        return 1.0;
    };
    let steps = range.evaluations_per_request();
    let step_ms = range.step_ms.max(1);
    match interval_ms {
        Some(interval_ms) => {
            let interval_ms = interval_ms.max(1);
            let shared_ms = least_common_multiple(interval_ms, step_ms);
            (shared_ms / step_ms).min(steps) as f64
        }
        None => steps as f64,
    }
}

/// The range one request of `entry` evaluates, as the data plane computes
/// it: its range evaluation; with an answer cache, only its
/// [`steps_computed_per_request`] new steps, a range whose span is that many
/// steps less one. `None` for an instant query. Only the steps the cost model counts per request take it
/// (rates, per-step fees, latency rounds, window reads): the entry itself,
/// its evaluation grid, request times and retention, is the user's.
///
/// Example: a 1 h range at 15 s steps every 20 s, with an answer cache: a
/// 45 s span at 15 s steps, 4 steps per request.
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
        assert_eq!(
            steps_computed_per_request(Some(range), Some(7_200_000)),
            241.0
        );
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
