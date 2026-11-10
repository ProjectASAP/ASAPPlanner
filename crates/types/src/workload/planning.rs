//! The planning workload: the query and data workloads planned together,
//! and why a workload is invalid.

use serde::{Deserialize, Serialize};

use super::query::MAX_RANGE_EVALUATION_STEPS;
use super::{DataWorkload, DurationMs, QueryWorkload};

/// Complete planner input. Query demand and source-data evidence are parallel
/// concerns and have independent provenance and update lifecycles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanningWorkload {
    pub query_workload: QueryWorkload,
    pub data_workload: Option<DataWorkload>,
}

impl PlanningWorkload {
    /// Validate the query workload and, when present, the data workload. A
    /// PromQL or MetricsQL sample interval is checked by the frontend, only
    /// for a query with a bare instant selector
    /// ([`WorkloadError::MissingSampleInterval`]).
    pub fn validate(&self) -> Result<(), WorkloadError> {
        self.query_workload.validate()?;
        if let Some(data) = &self.data_workload {
            data.validate()?;
        }
        Ok(())
    }

    /// The sample interval the data workload declares for the source
    /// `series` reads ([`DataWorkload::series_facts`]), if any.
    pub fn sample_interval_of(&self, series: &str) -> Option<DurationMs> {
        self.data_workload
            .as_ref()?
            .series_facts(series)?
            .0
            .sample_interval
    }
}

/// Why a planning workload is rejected. Every variant is bad input.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WorkloadError {
    /// Bad input: a PromQL or MetricsQL query has a bare instant selector,
    /// which reads the latest sample within its metric's sample interval,
    /// but the data workload declares no `sample_interval` for that metric.
    #[error(
        "a PromQL or MetricsQL instant selector needs its metric's sample_interval \
         in data_workload.sources"
    )]
    MissingSampleInterval,
    /// Bad input: a source declares a sample interval of zero.
    #[error("sample_interval must be greater than zero")]
    ZeroSampleInterval,
    /// Bad input: a one-time query with zero invocations.
    #[error("a one-time query must have at least one invocation")]
    ZeroInvocations,
    /// Bad input: a repeating query whose fixed interval is zero.
    #[error("a fixed repetition interval must be greater than zero")]
    ZeroRepetitionInterval,
    /// Bad input: a range evaluation whose `step_ms` is zero.
    #[error("a range evaluation's step_ms must be greater than zero")]
    ZeroRangeEvaluationStep,
    /// Bad input: a range evaluation whose `span_ms` is not a whole number
    /// of steps. Prometheus places steps forward from the start, so its
    /// last step would not be the request's end and would fall off the
    /// end's grid.
    #[error(
        "a range evaluation's span_ms ({span_ms}) must be a multiple of its step_ms ({step_ms})"
    )]
    RangeEvaluationSpanNotMultipleOfStep {
        /// The declared span.
        span_ms: u64,
        /// The declared step.
        step_ms: u64,
    },
    /// Bad input: a range evaluation with more steps per request than
    /// `MAX_RANGE_EVALUATION_STEPS`, which Prometheus would reject.
    #[error(
        "a range evaluation has {steps} steps per request, more than {}",
        MAX_RANGE_EVALUATION_STEPS
    )]
    TooManyRangeEvaluationSteps {
        /// The steps one request would evaluate, `span_ms / step_ms + 1`.
        steps: u64,
    },
    /// Bad input: a scheduled repeating query with no timestamps.
    #[error("a repeated-query schedule must not be empty")]
    EmptySchedule,
    /// Bad input: a rate (a demand estimate's, or the arrival's
    /// `rows_per_second`) that is negative or not finite.
    #[error("rate must be finite and non-negative, got {0}")]
    InvalidRate(f64),
    /// Bad input: a query's `max_latency_ms` that is negative or not
    /// finite.
    #[error("response latency must be finite and non-negative, got {0} ms")]
    InvalidLatency(f64),
    /// Bad input: a source's stream norm bound that is negative or not
    /// finite.
    #[error("a stream norm must be finite and non-negative, got {0}")]
    InvalidStreamNorm(f64),
    /// Bad input: a source's `max_distinct_per_evaluation` of zero.
    #[error("a source's max_distinct_per_evaluation must be greater than zero")]
    ZeroMaxDistinct,
    /// Bad input: a zero count in a source's `distinct_values_per_label`
    /// or `distinct_values_per_label_set`.
    #[error(
        "a source's distinct_values_per_label and distinct_values_per_label_set counts must be \
         greater than zero"
    )]
    ZeroDistinctValues,
    /// Bad input: a `distinct_values_per_label_set` entry naming fewer than
    /// two labels, or one label twice (one label's count belongs in
    /// `distinct_values_per_label`).
    #[error(
        "source {source_name}: a distinct_values_per_label_set entry must name two or more \
         distinct labels, got {labels:?}"
    )]
    LabelSetNeedsTwoLabels {
        source_name: String,
        labels: Vec<String>,
    },
    /// Bad input: a label set declaring more combinations than the product
    /// of its labels' declared distinct values allows.
    #[error(
        "source {source_name}: distinct_values_per_label_set declares {distinct_values} \
         combinations of {labels:?}, more than the {label_product} their \
         distinct_values_per_label counts allow"
    )]
    LabelSetCountAboveLabelProduct {
        source_name: String,
        labels: Vec<String>,
        distinct_values: u64,
        label_product: u64,
    },
    /// Bad input: a source's `rows_held` of zero.
    #[error("a source's rows_held must be greater than zero")]
    ZeroRowsHeld,
    /// Bad input: a source declares a `series_count` of zero. A source
    /// with no series has nothing to plan for, and `absent(m)` over it
    /// still returns one row.
    #[error("a source's series_count must be greater than zero")]
    ZeroSeriesCount,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::accuracy::AccuracyTarget;
    use crate::stages::ValueSign;
    use crate::workload::*;

    #[test]
    fn canonical_workloads_have_a_strict_json_round_trip() {
        let workload = PlanningWorkload {
            query_workload: QueryWorkload {
                language: QueryLanguage::PromQL,
                query_batch: None,
                repeating_queries: Some(vec![RepeatingEntry {
                    query: Query("rate(requests_total[5m])".into()),
                    demand: RepeatedDemand::FixedInterval(RepetitionInterval(10_000)),
                    requirements: QueryRequirements {
                        accuracy: AccuracyTarget::max_error(0.01).failure_probability(0.001),
                        max_latency_ms: Some(100.0),
                    },
                    predictability: Predictability::Predictable { known_at: None },
                    range_evaluation: None,
                }]),
            },
            data_workload: Some(DataWorkload {
                arrival: DataArrival::Ingesting {
                    rows_per_second: 1_000.0,
                    max_out_of_order_ms: None,
                },
                sources: BTreeMap::from([(
                    "requests_total".to_string(),
                    SourceFacts {
                        series_count: Some(10_000),
                        metric_type: Some(PrometheusMetricType::Counter),
                        column_signs: BTreeMap::from([(
                            "value".to_string(),
                            ValueSign::NonNegative,
                        )]),
                        sample_interval: Some(DurationMs(15_000)),
                        stream_norms: Some(StreamNorms {
                            l1: Some(1e9),
                            l2: None,
                        }),
                        max_distinct_per_evaluation: Some(100),
                        distinct_values_per_label: BTreeMap::from([
                            ("job".to_string(), 10),
                            ("instance".to_string(), 1_000),
                        ]),
                        distinct_values_per_label_set: vec![LabelSetDistinctValues {
                            labels: vec!["job".to_string(), "instance".to_string()],
                            distinct_values: 1_000,
                        }],
                        rows_held: Some(1_000_000),
                    },
                )]),
            }),
        };

        let json = serde_json::to_value(&workload).expect("serialize workload");
        assert_eq!(
            json["data_workload"],
            serde_json::json!({
                "arrival": {"ingesting": {"rows_per_second": 1000.0}},
                "sources": {"requests_total": {
                    "series_count": 10000,
                    "metric_type": "counter",
                    "column_signs": {"value": "non_negative"},
                    "sample_interval": 15000,
                    "stream_norms": {"l1": 1e9, "l2": null},
                    "max_distinct_per_evaluation": 100,
                    "distinct_values_per_label": {"instance": 1000, "job": 10},
                    "distinct_values_per_label_set": [
                        {"labels": ["job", "instance"], "distinct_values": 1000}
                    ],
                    "rows_held": 1000000
                }}
            })
        );
        let decoded: PlanningWorkload = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(decoded, workload);

        let text = serde_json::to_string(&workload).unwrap();
        let with_unknown = text.replacen("{", "{\"unknown\":true,", 1);
        assert!(serde_json::from_str::<PlanningWorkload>(&with_unknown).is_err());

        // Omitted facts default to unknown.
        let minimal: DataWorkload = serde_json::from_value(serde_json::json!({
            "arrival": {"mixed": {"rows_per_second": 5.0}},
            "sources": {"flows": {"series_count": 7}}
        }))
        .unwrap();
        assert_eq!(
            minimal.sources["flows"],
            SourceFacts {
                series_count: Some(7),
                ..SourceFacts::default()
            }
        );
    }
}
