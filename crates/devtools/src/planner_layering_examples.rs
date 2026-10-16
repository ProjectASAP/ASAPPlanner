//! The workloads of #509 Examples 1, 2, 3 and 4
//! (`docs/design_docs/proposals/planner-layering.md`), shared by
//! `stage_pipeline` and the planner-layering acceptance tests.

use asap_types::types::{AccuracyMetric, AccuracyTarget};
use asap_types::workload::{
    BatchEntry, DataArrival, DataWorkload, DurationMs, MetricType, PlanningWorkload,
    Predictability, Query, QueryLanguage, QueryRequirements, QueryWorkload, RepeatedDemand,
    RepeatingEntry, RepetitionInterval, SourceFacts, SqlDialect, TimestampMs,
};

pub const MINUTE_MS: u64 = 60_000;
/// PromQL's `y` is 365 days.
pub const YEAR_MS: u64 = 365 * 24 * 60 * MINUTE_MS;
/// Pattern A's batch execution time T (2026-01-01T00:00:00Z).
pub const T_MS: u64 = 1_767_225_600_000;

/// Rows per second the #509 shared data arrives at while ingesting:
/// 1,000,000 series, each sampled every 15 s.
pub const SHARED_ROWS_PER_SECOND: f64 = 1_000_000.0 / 15.0;

/// The #509 shared data, arriving as `arrival` says: one million series
/// of `source`, each sampled every 15 s.
pub fn shared_data_workload(arrival: DataArrival, source: &str) -> DataWorkload {
    DataWorkload {
        arrival,
        sources: [(
            source.to_string(),
            SourceFacts {
                series_count: Some(1_000_000),
                sample_interval: Some(DurationMs(15_000)),
                ..SourceFacts::default()
            },
        )]
        .into(),
    }
}

/// The metric both Example 1 queries read.
pub const EXAMPLE1_METRIC: &str = "http_requests_total";
pub const EXAMPLE1_Q1: &str = "sum by (job) (rate(http_requests_total[1m]))";
pub const EXAMPLE1_Q2: &str = "topk by (job) (10, sum_over_time(http_requests_total[1m]))";

/// Example 1: two dashboard panels, every 10 s over the last minute.
/// `http_requests_total` is declared a counter: its samples are never
/// negative.
pub fn example1() -> PlanningWorkload {
    let panel = |query: &str, accuracy, max_latency_ms| RepeatingEntry {
        query: Query(query.into()),
        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
        requirements: QueryRequirements {
            accuracy,
            max_latency_ms,
        },
        predictability: Predictability::Predictable { known_at: None },
    };
    PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::PromQL,
            query_batch: None,
            repeating_queries: Some(vec![
                panel(EXAMPLE1_Q1, AccuracyTarget::exact(), None),
                panel(
                    EXAMPLE1_Q2,
                    AccuracyTarget::max_error(0.01).failure_probability(0.001),
                    Some(100.0),
                ),
            ]),
        },
        data_workload: Some({
            let mut data = shared_data_workload(
                DataArrival::Ingesting {
                    rows_per_second: SHARED_ROWS_PER_SECOND,
                },
                EXAMPLE1_METRIC,
            );
            data.sources
                .get_mut(EXAMPLE1_METRIC)
                .expect("the shared source")
                .metric_type = Some(MetricType::Counter);
            data
        }),
    }
}

/// Example 2: three dashboard panels over one flow stream, every 10 s
/// over the last minute, with 10,000,000 distinct sources, and the
/// `flows(ts, src_ip)` table they read (the design names only these two
/// columns). The queries are the distinct sources (ε = 0.02), the entropy
/// of the per-source counts (ε = 0.05) and their L2 norm (ε = 0.01, the
/// strictest), each with δ = 0.01. Q3 casts the counts to DOUBLE: the
/// design's Int64 `c * c` can overflow, so the SQL frontend recognizes
/// only the Float64 product.
pub fn example2() -> (PlanningWorkload, asap_frontend_sql::SqlCatalog) {
    use asap_types::pre_asap::schema::{DataType, Field, Schema};
    // (query, ε, δ) per panel.
    let panels = [
        (
            "SELECT COUNT(DISTINCT src_ip) FROM flows WHERE ts >= now() - INTERVAL '1 minute'",
            0.02,
            0.01,
        ),
        (
            "SELECT -SUM(p * LN(p)) FROM (\
             SELECT COUNT(*) * 1.0 / SUM(COUNT(*)) OVER () AS p FROM flows \
             WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)",
            0.05,
            0.01,
        ),
        (
            "SELECT SQRT(SUM(CAST(c AS DOUBLE) * CAST(c AS DOUBLE))) FROM (\
             SELECT src_ip, COUNT(*) AS c FROM flows \
             WHERE ts >= now() - INTERVAL '1 minute' GROUP BY src_ip)",
            0.01,
            0.01,
        ),
    ];
    let workload = PlanningWorkload {
        query_workload: QueryWorkload {
            language: QueryLanguage::SQL(SqlDialect::DataFusionSQL),
            query_batch: None,
            repeating_queries: Some(
                panels
                    .iter()
                    .map(|&(query, epsilon, delta)| RepeatingEntry {
                        query: Query(query.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
                        requirements: QueryRequirements {
                            accuracy: AccuracyTarget::max_error(epsilon).failure_probability(delta),
                            max_latency_ms: None,
                        },
                        predictability: Predictability::Predictable { known_at: None },
                    })
                    .collect(),
            ),
        },
        // SQL needs no sample interval; `flows` holds 10,000,000 groups
        // (one per source IP).
        data_workload: Some(DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 1_000_000.0 / 15.0,
            },
            sources: [(
                "flows".to_string(),
                SourceFacts {
                    series_count: Some(10_000_000),
                    ..SourceFacts::default()
                },
            )]
            .into(),
        }),
    };
    let catalog = asap_frontend_sql::SqlCatalog::new().with_table(
        "flows",
        Schema::with_time_index(
            vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("src_ip", DataType::Utf8, false),
            ],
            0,
            vec![],
        ),
    );
    (workload, catalog)
}

/// The metric Example 3's Patterns A and B read.
pub const PATTERN_METRIC: &str = "latency_ms";

/// One of Pattern A's queries, with the window and offset its text states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatternAQuery {
    /// The PromQL text.
    pub promql: &'static str,
    /// Its range window, in ms.
    pub window_ms: u64,
    /// Its offset before T, in ms.
    pub offset_before_t_ms: u64,
}

/// Pattern A's five queries; the window and offset restate the query text.
pub const PATTERN_A: [PatternAQuery; 5] = [
    PatternAQuery {
        promql: "quantile_over_time(0.99, latency_ms[5y])",
        window_ms: 5 * YEAR_MS,
        offset_before_t_ms: 0,
    },
    PatternAQuery {
        promql: "quantile_over_time(0.99, latency_ms[1y])",
        window_ms: YEAR_MS,
        offset_before_t_ms: 0,
    },
    PatternAQuery {
        promql: "quantile_over_time(0.99, latency_ms[1y] offset 1y)",
        window_ms: YEAR_MS,
        offset_before_t_ms: YEAR_MS,
    },
    PatternAQuery {
        promql: "quantile_over_time(0.99, latency_ms[1y] offset 2y)",
        window_ms: YEAR_MS,
        offset_before_t_ms: 2 * YEAR_MS,
    },
    PatternAQuery {
        promql: "quantile_over_time(0.99, latency_ms[3y] offset 2y)",
        window_ms: 3 * YEAR_MS,
        offset_before_t_ms: 2 * YEAR_MS,
    },
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
        accuracy: AccuracyTarget::max_error(0.005)
            .failure_probability(0.01)
            .accuracy_metric(AccuracyMetric::Rank),
        max_latency_ms: None,
    }
}

/// Example 3, Pattern A: p99 over five historical intervals.
pub fn pattern_a(recurrence: PatternARecurrence, arrival: DataArrival) -> PlanningWorkload {
    let (batch, repeating) = match recurrence {
        PatternARecurrence::OnceAdHoc => (
            Some(
                PATTERN_A
                    .iter()
                    .map(|pattern| BatchEntry {
                        query: Query(pattern.promql.into()),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::AdHoc,
                        invocations: 1,
                        execute_at: Some(TimestampMs(T_MS)),
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
                    .map(|pattern| RepeatingEntry {
                        query: Query(pattern.promql.into()),
                        demand: RepeatedDemand::FixedInterval(RepetitionInterval(
                            (30 * 24 * 60 * MINUTE_MS) as u32,
                        )),
                        requirements: pattern_a_requirements(),
                        predictability: Predictability::Predictable {
                            known_at: Some(TimestampMs(T_MS)),
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
        data_workload: Some(shared_data_workload(arrival, PATTERN_METRIC)),
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
                    accuracy: AccuracyTarget::max_error(0.01)
                        .failure_probability(0.01)
                        .accuracy_metric(AccuracyMetric::Rank),
                    max_latency_ms: Some(200.0),
                },
                predictability: Predictability::Predictable { known_at: None },
            }]),
        },
        data_workload: Some(shared_data_workload(
            DataArrival::Ingesting {
                rows_per_second: SHARED_ROWS_PER_SECOND,
            },
            PATTERN_METRIC,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shared arrival validates, at rest included.
    #[test]
    fn shared_data_workloads_are_valid() {
        for arrival in [
            DataArrival::AtRest,
            DataArrival::Ingesting {
                rows_per_second: SHARED_ROWS_PER_SECOND,
            },
            DataArrival::Mixed {
                rows_per_second: SHARED_ROWS_PER_SECOND,
            },
        ] {
            shared_data_workload(arrival, PATTERN_METRIC)
                .validate()
                .unwrap();
            pattern_a(PatternARecurrence::OnceAdHoc, arrival)
                .validate()
                .unwrap();
        }
    }
}
