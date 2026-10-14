//! The workloads of #509 Examples 1, 3 and 4
//! (`docs/design_docs/proposals/planner-layering.md`), shared by
//! `stage_pipeline` and the planner-layering acceptance tests.

use asap_types::types::AccuracyTarget;
use asap_types::workload::{
    AccuracyRequirement, BatchEntry, DataArrival, DataDistribution, DataWorkload, DurationMs,
    Evidence, EvidenceSource, LatencyRequirement, MetricType, PlanningWorkload, Predictability,
    Query, QueryLanguage, QueryRequirements, QueryTimeScope, QueryWorkload, Rate, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, TimeSelection, TimestampMs,
};

pub fn declared<T>(value: T) -> Evidence<T> {
    Evidence {
        value: Some(value),
        source: EvidenceSource::Declared,
        ..Default::default()
    }
}

pub const MINUTE_MS: u64 = 60_000;
/// PromQL's `y` is 365 days.
pub const YEAR_MS: u64 = 365 * 24 * 60 * MINUTE_MS;
/// Pattern A's batch execution time T (2026-01-01T00:00:00Z).
pub const T_MS: u64 = 1_767_225_600_000;

/// The #509 shared data workload with `arrival`.
pub fn shared_data_workload(arrival: DataArrival) -> DataWorkload {
    DataWorkload {
        data_ingestion_interval: declared(DurationMs(15_000)),
        ingestion_volume: Evidence::default(),
        // Data at rest has no ingestion (`validate` rejects a positive rate).
        ingestion_rate: match arrival {
            DataArrival::AtRest => Evidence::default(),
            _ => declared(Rate(1_000_000.0 / 15.0)),
        },
        input_cardinality: declared(1_000_000),
        distribution: declared(DataDistribution::Zipf),
        stream_norms: Evidence::default(),
        arrival,
        metric_types: Default::default(),
    }
}

pub const EXAMPLE1_Q1: &str = "sum by (job) (rate(http_requests_total[1m]))";
pub const EXAMPLE1_Q2: &str = "topk by (job) (10, sum_over_time(http_requests_total[1m]))";

/// Example 1: two dashboard panels, every 10 s over the last minute.
/// `http_requests_total` is declared a counter: its samples are never
/// negative.
pub fn example1() -> PlanningWorkload {
    let panel = |query: &str, accuracy, response_latency| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy: AccuracyRequirement::Explicit(accuracy),
            response_latency,
        },
        predictability: Predictability::Predictable { known_at: None },
        time_selection: TimeSelection {
            scope: QueryTimeScope::RealTime,
            lookback: Some(DurationMs(MINUTE_MS)),
            as_of: None,
        },
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                panel(
                    EXAMPLE1_Q1,
                    AccuracyTarget::Exact,
                    LatencyRequirement::Unspecified,
                ),
                panel(
                    EXAMPLE1_Q2,
                    AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.001,
                    },
                    LatencyRequirement::ExplicitMaxMs(100.0),
                ),
            ]),
        },
        data_workload: Some(DataWorkload {
            metric_types: [("http_requests_total".to_string(), MetricType::Counter)].into(),
            ..shared_data_workload(DataArrival::ContinuouslyIngesting)
        }),
    }
}

/// Pattern A's five queries: (PromQL, `lookback`, T − `as_of`).
pub const PATTERN_A: [(&str, u64, u64); 5] = [
    ("quantile_over_time(0.99, latency_ms[5y])", 5 * YEAR_MS, 0),
    ("quantile_over_time(0.99, latency_ms[1y])", YEAR_MS, 0),
    (
        "quantile_over_time(0.99, latency_ms[1y] offset 1y)",
        YEAR_MS,
        YEAR_MS,
    ),
    (
        "quantile_over_time(0.99, latency_ms[1y] offset 2y)",
        YEAR_MS,
        2 * YEAR_MS,
    ),
    (
        "quantile_over_time(0.99, latency_ms[3y] offset 2y)",
        3 * YEAR_MS,
        2 * YEAR_MS,
    ),
];

/// How Pattern A's batch recurs (Example 4 varies it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternARecurrence {
    /// As given: one `ad_hoc` batch run once at T.
    OnceAdHoc,
    /// Example 4: repeated monthly and `Predictable { known_at }`; each run
    /// reads the intervals ending at its own evaluation time.
    MonthlyPredictable,
}

fn pattern_a_requirements() -> QueryRequirements {
    QueryRequirements {
        accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
            epsilon: 0.005,
            delta: 0.01,
        }),
        response_latency: LatencyRequirement::Unspecified,
    }
}

/// Example 3, Pattern A: p99 over five historical intervals.
pub fn pattern_a(recurrence: PatternARecurrence, arrival: DataArrival) -> PlanningWorkload {
    let (batch, repeating) = match recurrence {
        PatternARecurrence::OnceAdHoc => (
            Some(
                PATTERN_A
                    .iter()
                    .map(|&(query, lookback, before_t)| BatchEntry {
                        query: Query(query.into()),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::AdHoc,
                        invocations: 1,
                        execute_at: Some(TimestampMs(T_MS)),
                        time_selection: TimeSelection {
                            scope: QueryTimeScope::Longitudinal,
                            lookback: Some(DurationMs(lookback)),
                            as_of: Some(TimestampMs(T_MS - before_t)),
                        },
                    })
                    .collect(),
            ),
            None,
        ),
        PatternARecurrence::MonthlyPredictable => (
            None,
            Some(
                PATTERN_A
                    .iter()
                    .map(|&(query, lookback, _)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                            (30 * 24 * 60 * MINUTE_MS) as u32,
                        )),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::Predictable {
                            known_at: Some(TimestampMs(T_MS)),
                        },
                        time_selection: TimeSelection {
                            scope: QueryTimeScope::Longitudinal,
                            lookback: Some(DurationMs(lookback)),
                            as_of: None,
                        },
                    })
                    .collect(),
            ),
        ),
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: batch,
            repeating_queries: repeating,
        },
        data_workload: Some(shared_data_workload(arrival)),
    }
}

pub const PATTERN_B: &str = "quantile_over_time(0.99, latency_ms[5m])";
pub const PATTERN_B_WINDOW_MS: u64 = 5 * MINUTE_MS;
pub const PATTERN_B_INTERVAL_MS: u64 = MINUTE_MS;

/// Example 3, Pattern B: a p99 panel over the last 5 min, every minute.
pub fn pattern_b() -> PlanningWorkload {
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![RepeatingEntry {
                query: Query(PATTERN_B.into()),
                demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                    PATTERN_B_INTERVAL_MS as u32,
                )),
                requirements: QueryRequirements {
                    accuracy: AccuracyRequirement::Explicit(AccuracyTarget::EpsilonDelta {
                        epsilon: 0.01,
                        delta: 0.01,
                    }),
                    response_latency: LatencyRequirement::ExplicitMaxMs(200.0),
                },
                predictability: Predictability::Predictable { known_at: None },
                time_selection: TimeSelection {
                    scope: QueryTimeScope::RealTime,
                    lookback: Some(DurationMs(PATTERN_B_WINDOW_MS)),
                    as_of: None,
                },
            }]),
        },
        data_workload: Some(shared_data_workload(DataArrival::ContinuouslyIngesting)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Data at rest has no ingestion rate, so the at-rest shared data
    /// workload validates.
    #[test]
    fn at_rest_shared_data_workload_is_valid() {
        shared_data_workload(DataArrival::AtRest)
            .validate()
            .unwrap();
        pattern_a(PatternARecurrence::OnceAdHoc, DataArrival::AtRest)
            .validate()
            .unwrap();
    }
}
