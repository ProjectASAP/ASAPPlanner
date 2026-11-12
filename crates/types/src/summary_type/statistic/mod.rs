//! What a summary is updated with and the statistics read from it.

use serde::{Deserialize, Serialize};

use crate::ir::operator::arguments::Source;
use crate::ir::operator::input::OperatorInput;
use crate::ir::operator::AggIntent;
use crate::ir::scalar::ColumnRef;
use crate::ir::{NonASAPOp, Operator};

/// One summary-state update. `item` identifies a keyed item when the family
/// is keyed; `weight` is the observation/update expression applied to state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SummaryUpdate {
    pub item: Option<SummaryInputExpr>,
    pub weight: SummaryInputExpr,
    /// Planner proof for algorithms whose error/update semantics require a
    /// non-negative stream. Absence of proof is never interpreted as zero or
    /// non-negative.
    #[serde(default)]
    pub weight_domain: WeightDomain,
}

impl SummaryUpdate {
    pub fn column(column: ColumnRef) -> Self {
        Self {
            item: None,
            weight: SummaryInputExpr::Column(column),
            weight_domain: WeightDomain::UnknownOrSigned,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WeightDomain {
    #[default]
    UnknownOrSigned,
    NonNegative {
        proof: NonNegativeWeightProof,
    },
    /// An open requirement, not a proof: the weights are raw data samples of
    /// `metric`, or plain sums of them (no rate, no other arithmetic), so they
    /// are non-negative when every raw data sample of `metric` is: `metric` is
    /// declared a counter, or declared non-negative. Logical optimization
    /// records it without reading the data workload; plan selection confirms
    /// it from the declared source facts
    /// ([`SourceFacts::metric_type`](crate::workload::SourceFacts::metric_type),
    /// [`SourceFacts::column_signs`](crate::workload::SourceFacts::column_signs)).
    /// Unconfirmed, it counts as not proven. Serialized as
    /// `samples_of_metric`.
    #[serde(rename = "samples_of_metric")]
    RawDataSamplesOfMetric {
        metric: String,
    },
    /// An open requirement, not a proof: the weights are reset-corrected
    /// `rate`, `increase` or `irate` values of `metric`'s raw data samples, or plain
    /// sums of them. Reset correction keeps them non-negative exactly when
    /// the samples are, that is when `metric` is a counter; over a gauge a
    /// negative sample can give a negative value. Plan selection confirms
    /// it from the declared metric types, as
    /// [`RawDataSamplesOfMetric`](Self::RawDataSamplesOfMetric). Unconfirmed, it
    /// counts as not proven.
    CounterDerivativeOfMetric {
        metric: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NonNegativeWeightProof {
    UnitCount,
    /// `rate`, `increase` or `irate` of a counter's samples, or plain sums
    /// of them: reset correction over non-negative samples gives a
    /// non-negative increase, and a rate divides it by a positive duration.
    /// Plan selection gives it on confirming a
    /// [`WeightDomain::CounterDerivativeOfMetric`] requirement.
    ResetAwareCounterDerivative,
    /// Raw data samples of a metric declared a counter
    /// ([`PrometheusMetricType::Counter`](crate::workload::PrometheusMetricType::Counter)), or
    /// plain sums of them: a counter sample is never negative. Plan selection
    /// gives it on confirming a
    /// [`WeightDomain::RawDataSamplesOfMetric`] requirement. Serialized as
    /// `counter_samples`.
    #[serde(rename = "counter_samples")]
    CounterRawDataSamples,
    /// Raw data samples of a metric declared non-negative
    /// ([`SourceFacts::column_signs`](crate::workload::SourceFacts::column_signs)),
    /// such as a gauge, or plain sums of them. Plan selection gives it on
    /// confirming a [`WeightDomain::RawDataSamplesOfMetric`] requirement for a
    /// metric not declared a counter. Serialized as
    /// `declared_non_negative_samples`.
    #[serde(rename = "declared_non_negative_samples")]
    DeclaredNonNegativeRawDataSamples,
    /// A constant weight greater than 0, such as `Constant(2.0)`: every row
    /// adds the same positive amount, so no weight is negative.
    PositiveConstant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntityIdentity {
    PromqlLabelSet { excluding: Vec<ColumnRef> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SummaryInputExpr {
    Constant(f64),
    Column(ColumnRef),
    Tuple(Vec<SummaryInputExpr>),
    EntityIdentity(EntityIdentity),
}

/// The statistic computed from summary state by `SummaryEstimate`, for example
/// `Quantile { q: 0.99 }`. This is a result operation, not a workload query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SketchStatistic {
    /// sqrt(sum_v frequency(v)^2), not the norm of numeric input values.
    FrequencyL2,
    /// Shannon entropy of the value-frequency distribution, in bits.
    FrequencyEntropy,
    /// Extract the value at quantile rank `q` ∈ (0, 1].
    Quantile { q: f64 },
    /// Estimated count / frequency. `key` names which column is being
    /// queried (`ColumnRef::SampleValue` for the bare bucket total, with
    /// `value: None`); a `Named`/`Qualified` `key` paired with
    /// `value: Some(v)` is a per-item point lookup (e.g.
    /// `count(cms_metric{item="checkout"})` — `key` is `item`, `value` is
    /// `"checkout"`). `value` is carried here rather than resolved by the
    /// `SummaryExecutor` from a `Filter` predicate because a summary
    /// evaluation is read without access to the rest of the DAG.
    PointCount {
        key: ColumnRef,
        value: Option<String>,
    },
    /// Estimated number of distinct elements.
    Cardinality,
    /// Read the top-k entries from a heap-backed summary.
    TopK { k: usize },
}

impl SketchStatistic {
    /// The statistic a summary is read for to answer `intent`, or `None`
    /// when no sketch statistic serves the intent.
    pub fn for_intent(intent: &AggIntent) -> Option<Self> {
        Some(match intent {
            AggIntent::Quantile { q, .. } => Self::Quantile { q: *q },
            AggIntent::Cardinality { .. } => Self::Cardinality,
            AggIntent::FrequencyL2 { .. } => Self::FrequencyL2,
            AggIntent::FrequencyEntropy { .. } => Self::FrequencyEntropy,
            AggIntent::TopK { k, .. } => Self::TopK { k: *k },
            AggIntent::Count => Self::PointCount {
                key: ColumnRef::SampleValue,
                value: None,
            },
            _ => return None,
        })
    }
}

/// The metric whose raw data samples, or plain sums of them, are every value
/// `input` outputs, or `None`: walking down through a time range, an
/// `offset`-only time shift (it moves the window, not the samples) and a
/// plain `sum`, to a time-series scan. Any other operator (rate and
/// arithmetic included) ends it. The metric's type is not read here: whether
/// its samples are non-negative is plan selection's question
/// ([`WeightDomain::RawDataSamplesOfMetric`]).
pub fn raw_data_samples_of_metric<I: OperatorInput>(input: &I) -> Option<String> {
    let Operator::NonASAP(operator) = input.operator() else {
        return None;
    };
    match operator {
        NonASAPOp::Scan {
            source: Source::TimeSeries { metric },
            ..
        } => Some(metric),
        NonASAPOp::TimeRange { child, .. } => raw_data_samples_of_metric(&child),
        NonASAPOp::TimeShift { shift, child } if shift.at.is_none() => {
            raw_data_samples_of_metric(&child)
        }
        NonASAPOp::Aggregate {
            measures, child, ..
        } if matches!(measures.as_slice(), [AggIntent::Sum { col: None }]) => {
            raw_data_samples_of_metric(&child)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::arguments::Reduction;
    use crate::ir::schema::{DataType, Field, Schema};
    use crate::ir::{OperatorNode, TimeRangeKind};
    use std::rc::Rc;
    use std::time::Duration;

    /// The raw data samples of `m` reach a node through a range, an
    /// `offset`-only time shift and a plain `sum`; a rate ends them.
    #[test]
    fn raw_data_samples_of_metric_follows_ranges_shifts_and_plain_sums() {
        let node =
            |operator: Operator<Rc<OperatorNode>>| OperatorNode::new_shared(operator).unwrap();
        let rows = node(Operator::NonASAP(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                ],
                0,
                vec![],
            ),
        }));
        let window = node(Operator::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(60),
            kind: TimeRangeKind::Range,
            child: rows,
        }));
        let per_series = |measure: AggIntent| {
            node(Operator::NonASAP(NonASAPOp::Aggregate {
                child: Rc::clone(&window),
                reduction: Reduction::PerEntity,
                measures: vec![measure],
                output_names: vec![String::new()],
                filters: vec![],
                having: None,
            }))
        };
        assert_eq!(raw_data_samples_of_metric(&window), Some("m".to_string()));
        let shifted = node(Operator::NonASAP(NonASAPOp::TimeShift {
            shift: crate::ir::operator::arguments::TimeShift {
                offset_ms: 60_000,
                at: None,
            },
            child: Rc::clone(&window),
        }));
        assert_eq!(raw_data_samples_of_metric(&shifted), Some("m".to_string()));
        assert_eq!(
            raw_data_samples_of_metric(&per_series(AggIntent::Sum { col: None })),
            Some("m".to_string())
        );
        assert_eq!(
            raw_data_samples_of_metric(&per_series(AggIntent::Rate)),
            None
        );
    }

    #[test]
    fn keyed_summary_input_and_topk_evaluation_round_trip() {
        for input in [
            SummaryUpdate {
                item: Some(SummaryInputExpr::EntityIdentity(
                    EntityIdentity::PromqlLabelSet { excluding: vec![] },
                )),
                weight: SummaryInputExpr::Constant(1.0),
                weight_domain: WeightDomain::NonNegative {
                    proof: NonNegativeWeightProof::UnitCount,
                },
            },
            SummaryUpdate {
                item: Some(SummaryInputExpr::Tuple(vec![SummaryInputExpr::Column(
                    ColumnRef::Named("service".into()),
                )])),
                weight: SummaryInputExpr::Column(ColumnRef::SampleValue),
                weight_domain: WeightDomain::UnknownOrSigned,
            },
        ] {
            let input_json = serde_json::to_string(&input).unwrap();
            assert_eq!(
                serde_json::from_str::<SummaryUpdate>(&input_json).unwrap(),
                input
            );
            let query = SketchStatistic::TopK { k: 10 };
            let json = serde_json::to_string(&query).unwrap();
            assert_eq!(
                serde_json::from_str::<SketchStatistic>(&json).unwrap(),
                query
            );
        }
    }

    /// The raw-data-sample and summary-sample renames keep their JSON names,
    /// so committed plans and workloads still read.
    #[test]
    fn renamed_sample_terms_keep_their_wire_names() {
        use crate::ir::schema::FieldDataType;
        use crate::summary_type::{SamplingKind, SamplingParams};
        let json = |value: serde_json::Value| value.to_string();
        let raw = WeightDomain::RawDataSamplesOfMetric {
            metric: "m".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&raw).unwrap(),
            json(serde_json::json!({"kind": "samples_of_metric", "metric": "m"}))
        );
        for (proof, name) in [
            (
                NonNegativeWeightProof::CounterRawDataSamples,
                "counter_samples",
            ),
            (
                NonNegativeWeightProof::DeclaredNonNegativeRawDataSamples,
                "declared_non_negative_samples",
            ),
        ] {
            assert_eq!(
                serde_json::to_string(&proof).unwrap(),
                format!("\"{name}\"")
            );
        }
        let summary_sample = FieldDataType::SummarySample(
            SamplingKind::Reservoir,
            SamplingParams::Reservoir { size: 64 },
        );
        assert_eq!(
            serde_json::to_string(&summary_sample).unwrap(),
            json(serde_json::json!({"Sample": ["Reservoir", {"Reservoir": {"size": 64}}]}))
        );
    }
}
