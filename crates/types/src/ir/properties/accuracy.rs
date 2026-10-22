//! Accuracy requirements and the error metrics they are stated in (#690).
//!
//! A requirement and a guarantee share one outer shape, [`AccuracySpec`]:
//! an error metric, an error ε, a failure probability δ and a scope. A
//! requirement ([`AccuracyTarget`]) holds concrete numbers and may leave its
//! metric unset; a guarantee ([`crate::ir::properties::AccuracyGuarantee`])
//! always states its metric and holds symbolic expressions that plan
//! selection binds. A target without a metric accepts a guarantee in any
//! metric that meets its ε, δ and scope.

use serde::{Deserialize, Serialize};

use crate::ir::operator::AggIntent;
use crate::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};

/// The stream norm an [`AccuracyMetric::Absolute`] error is stated in
/// multiples of. A scaled error `ε` means `|estimate − truth| ≤ ε · ‖f‖`,
/// where `f` is the frequency vector of the one stream the estimate's
/// summaries read: `‖f‖₁` for Count-Min, `‖f‖₂` for Count Sketch. Two scaled
/// errors compare directly only over the same stream; restating one in value
/// units multiplies it by that stream's norm, a statistic plan selection
/// binds from the data workload (`stream_l1_norm[source]`,
/// `stream_l2_norm[source]`). Since `‖f‖₂ ≤ ‖f‖₁`, an L2-scaled error meets
/// an L1-scaled requirement of the same stream with the same ε.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NormScale {
    StreamL1,
    StreamL2,
}

/// Which error quantity an accuracy statement bounds. `#[non_exhaustive]`:
/// a metric is added only when a primitive needs a notion these cannot
/// express (e.g. a false-positive rate), and downstream matches must not
/// assume the list is closed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "MetricName", from = "MetricName")]
pub enum AccuracyMetric {
    /// `|estimate − truth| ≤ ε`: in the value's own units when `scale` is
    /// `None`, or in multiples of a stream norm ([`NormScale`]).
    Absolute { scale: Option<NormScale> },
    /// `|estimate − truth| ≤ ε · |truth|`, e.g. DDSketch's α or a distinct
    /// count's relative error.
    Relative,
    /// The returned value's rank in the input multiset is within `ε · n` of
    /// the requested rank (a quantile's KLL ε). Says nothing about how far
    /// the returned value is from the true quantile value.
    Rank,
}

impl AccuracyMetric {
    /// Absolute error in the value's own units.
    pub const ABSOLUTE: Self = Self::Absolute { scale: None };
    /// Absolute error in multiples of the stream's L1 norm (Count-Min).
    pub const ABSOLUTE_STREAM_L1: Self = Self::Absolute {
        scale: Some(NormScale::StreamL1),
    };
    /// Absolute error in multiples of the stream's L2 norm (Count Sketch).
    pub const ABSOLUTE_STREAM_L2: Self = Self::Absolute {
        scale: Some(NormScale::StreamL2),
    };
}

/// The wire name of an [`AccuracyMetric`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MetricName {
    Absolute,
    AbsoluteStreamL1,
    AbsoluteStreamL2,
    Relative,
    Rank,
}

impl From<AccuracyMetric> for MetricName {
    fn from(metric: AccuracyMetric) -> Self {
        match metric {
            AccuracyMetric::Absolute { scale: None } => Self::Absolute,
            AccuracyMetric::Absolute {
                scale: Some(NormScale::StreamL1),
            } => Self::AbsoluteStreamL1,
            AccuracyMetric::Absolute {
                scale: Some(NormScale::StreamL2),
            } => Self::AbsoluteStreamL2,
            AccuracyMetric::Relative => Self::Relative,
            AccuracyMetric::Rank => Self::Rank,
        }
    }
}

impl From<MetricName> for AccuracyMetric {
    fn from(name: MetricName) -> Self {
        match name {
            MetricName::Absolute => Self::ABSOLUTE,
            MetricName::AbsoluteStreamL1 => Self::ABSOLUTE_STREAM_L1,
            MetricName::AbsoluteStreamL2 => Self::ABSOLUTE_STREAM_L2,
            MetricName::Relative => Self::Relative,
            MetricName::Rank => Self::Rank,
        }
    }
}

/// Which output values one (ε, δ) statement covers *simultaneously*.
///
/// A scope bounds only the failure probability: a per-value guarantee with
/// failure probability `δ` holds over the `n` values a scope covers with
/// `n · δ` by the union bound ([`Self::max_values_covered`]), whatever their
/// dependence. Not `Eq`: a record share is an `f64`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccuracyScope {
    /// Each output value (one series or row, at one evaluation) is within ε
    /// of its true value with probability ≥ 1 − δ, separately. Values may
    /// fail in different runs; nothing is said about all of them at once.
    /// Sketches and composition produce guarantees of this scope.
    PerValue,
    /// Every value in one evaluation's returned result is within ε
    /// simultaneously, with probability ≥ 1 − δ. The default of a target: a
    /// target at a query root applies to the entire returned result.
    WholeResult,
    /// Every group (result row) holding at least this share of the records
    /// the query aggregates in one evaluation is within ε simultaneously,
    /// with probability ≥ 1 − δ; smaller groups are not covered. The share
    /// is a fraction in (0, 1] (0.05 for 5%). When a row reads several
    /// aggregations, its group must hold the share of each one's records.
    /// At most `⌊1 / share⌋` groups can qualify, whatever the number of
    /// groups. JSON: `{"groups_with_record_share_at_least": 0.05}`.
    GroupsWithRecordShareAtLeast(#[serde(deserialize_with = "record_share")] f64),
}

/// Relative tolerance for `⌊1 / share⌋`, so a share such as 0.05 whose
/// reciprocal computes just below an integer counts that integer.
const RECORD_SHARE_ROUNDING_TOLERANCE: f64 = 1e-9;

/// A record share, rejected unless it is a fraction in (0, 1].
fn record_share<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let share = f64::deserialize(deserializer)?;
    if share > 0.0 && share <= 1.0 {
        Ok(share)
    } else {
        Err(serde::de::Error::custom(format!(
            "a record share is a fraction in (0, 1], got {share}"
        )))
    }
}

impl AccuracyScope {
    /// How many values one evaluation's statement over this scope covers at
    /// most, given the `values_per_result` (`G`) the result returns, `None`
    /// when unbound: one for [`Self::PerValue`], `G` for
    /// [`Self::WholeResult`], and `min(⌊1 / share⌋, G)` for
    /// [`Self::GroupsWithRecordShareAtLeast`], which is bound even when `G`
    /// is not. Sizing divides a target's failure probability by it, and the
    /// guarantee check multiplies a per-value one by it, so the strictest of
    /// several scopes is the one that covers the most values.
    pub fn max_values_covered(self, values_per_result: Option<u64>) -> Option<u64> {
        match self {
            Self::PerValue => Some(1),
            Self::WholeResult => values_per_result,
            // A share outside (0, 1] (possible only through the Rust API;
            // JSON rejects it) bounds nothing, so its target stays unmet.
            Self::GroupsWithRecordShareAtLeast(share) if !(share > 0.0 && share <= 1.0) => None,
            Self::GroupsWithRecordShareAtLeast(share) => {
                // More than `1 / share` groups cannot each hold `share` of
                // the records. Rounding up near an integer keeps `1 / 0.05`
                // at 20 when it computes just below; it only enlarges the
                // count, which is conservative.
                let qualifying =
                    (share.recip() * (1.0 + RECORD_SHARE_ROUNDING_TOLERANCE)).floor() as u64;
                Some(values_per_result.map_or(qualifying, |values| values.min(qualifying)))
            }
        }
    }

    /// Whether a statement over this scope also covers every value `other`
    /// covers: the whole result covers every scope, a record share covers a
    /// larger share (not every value: a smaller group is outside it), and a
    /// single value only itself.
    pub fn covers(self, other: Self) -> bool {
        match (self, other) {
            (Self::WholeResult, _) | (Self::PerValue, Self::PerValue) => true,
            (
                Self::GroupsWithRecordShareAtLeast(share),
                Self::GroupsWithRecordShareAtLeast(other_share),
            ) => share <= other_share,
            _ => false,
        }
    }
}

/// The outer shape of an accuracy guarantee
/// ([`crate::ir::properties::AccuracyGuarantee`]):
/// `Pr[err_metric(estimate, truth) > error] ≤ failure_probability` over the
/// values `accuracy_scope` names. An [`AccuracyTarget`] has the same fields, with
/// concrete numbers, an optional metric and `max_error` for `error`: a target
/// states the largest error a query accepts, a guarantee the error bound
/// (an expression over the plan's parameters) its plan achieves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracySpec<E, P> {
    pub accuracy_metric: AccuracyMetric,
    /// ε: the error bound, in `accuracy_metric`'s units.
    pub error: E,
    /// δ.
    pub failure_probability: P,
    pub accuracy_scope: AccuracyScope,
}

/// An accuracy requirement, as a workload, a caller or a frontend states
/// it: `Pr[err_metric(estimate, truth) > max_error] ≤ failure_probability` over
/// the values `accuracy_scope` names.
///
/// `accuracy_metric: None` means the user did not set one: plan selection
/// then accepts a guarantee in any metric whose ε, δ and scope meet this
/// target's, and reports the metric it accepted. A set metric accepts only
/// guarantees in that metric (or convertible to it with evidence).
/// `failure_probability: None` imposes no δ requirement (sizing then uses
/// the planner's default δ). Exact is `max_error = 0`,
/// `failure_probability = Some(0)`, whatever the metric.
///
/// Built with [`Self::exact`] or [`Self::max_error`] plus the optional
/// builders [`Self::failure_probability`], [`Self::accuracy_metric`] and
/// [`Self::accuracy_scope`].
///
/// JSON: `{"accuracy_metric": "rank", "max_error": 0.01, "failure_probability":
/// 0.001, "accuracy_scope": "whole_result"}`; every field but `max_error` may be
/// omitted (`accuracy_scope` defaults to `whole_result`). Exact is
/// `{"max_error": 0.0, "failure_probability": 0.0}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracyTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy_metric: Option<AccuracyMetric>,
    /// ε: the largest error the query accepts, in the metric's units.
    pub max_error: f64,
    /// δ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_probability: Option<f64>,
    #[serde(default = "whole_result")]
    pub accuracy_scope: AccuracyScope,
}

fn whole_result() -> AccuracyScope {
    AccuracyScope::WholeResult
}

impl AccuracyTarget {
    /// No approximation permitted.
    pub const fn exact() -> Self {
        Self {
            accuracy_metric: None,
            max_error: 0.0,
            failure_probability: Some(0.0),
            accuracy_scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `max_error` over the whole result, with no δ
    /// requirement and no metric set.
    pub const fn max_error(max_error: f64) -> Self {
        Self {
            accuracy_metric: None,
            max_error,
            failure_probability: None,
            accuracy_scope: AccuracyScope::WholeResult,
        }
    }

    /// This target, met with probability at least `1 − failure_probability`.
    pub const fn failure_probability(self, failure_probability: f64) -> Self {
        Self {
            failure_probability: Some(failure_probability),
            ..self
        }
    }

    /// This target with its error stated in `accuracy_metric`.
    pub const fn accuracy_metric(self, accuracy_metric: AccuracyMetric) -> Self {
        Self {
            accuracy_metric: Some(accuracy_metric),
            ..self
        }
    }

    /// This target over `accuracy_scope`.
    pub const fn accuracy_scope(self, accuracy_scope: AccuracyScope) -> Self {
        Self {
            accuracy_scope,
            ..self
        }
    }

    /// Whether this target permits no approximation.
    pub fn is_exact(&self) -> bool {
        self.max_error == 0.0 && self.failure_probability == Some(0.0)
    }
}

/// The metric a query's result is most naturally stated in, from the
/// computation at `root`: an L1-scaled absolute error for counts and top-k
/// frequencies, a relative error for distinct counts and L2 frequency
/// moments, and an absolute value error for anything else (sums,
/// arithmetic, rates, averages, extrema, entropy). `None` for a quantile
/// root, whose error is as naturally a rank as a relative one.
///
/// Used only to size and to display: a target whose metric is not set
/// accepts a guarantee in any metric (#690).
pub fn default_metric_for_root(root: &QueryRoot) -> Option<AccuracyMetric> {
    match root {
        QueryRoot::Operator(node) => node_metric(node),
        QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node))
        | QueryRoot::Scalar(ScalarExpr::ScalarSubquery(node)) => node_metric(node),
        QueryRoot::Scalar(_) => Some(AccuracyMetric::ABSOLUTE),
    }
}

/// The default metric of an intent's value, `None` for a quantile and for
/// intents without a listed default.
fn intent_metric(intent: &AggIntent) -> Option<AccuracyMetric> {
    match intent {
        AggIntent::Count { .. } | AggIntent::TopK { .. } => {
            Some(AccuracyMetric::ABSOLUTE_STREAM_L1)
        }
        AggIntent::Cardinality { .. } | AggIntent::FrequencyL2 { .. } => {
            Some(AccuracyMetric::Relative)
        }
        _ => None,
    }
}

/// The metric of `node`'s values, looking through operators that only
/// select, order or rename rows.
fn node_metric(node: &OperatorNode) -> Option<AccuracyMetric> {
    let Operator::NonASAP(op) = &node.operator else {
        return Some(AccuracyMetric::ABSOLUTE);
    };
    match op {
        NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::Dedup { child, .. } => node_metric(child),
        NonASAPOp::Project { cols, child, .. }
            if cols
                .iter()
                .all(|item| matches!(item.expr, ScalarExpr::Column(_))) =>
        {
            node_metric(child)
        }
        // A `CASE` choosing between columns and literals, e.g. SQL's
        // empty-input NULL guard over a statistic cross-joined with an exact
        // count: the metric of the columns its value branches read.
        NonASAPOp::Project { .. } => (0..node.schema.fields.len())
            .map(|column| column_metric(node, column))
            .collect::<Option<Vec<_>>>()
            .map(|metrics| match metrics.split_first() {
                Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                _ => AccuracyMetric::ABSOLUTE,
            }),
        NonASAPOp::Aggregate { measures, .. } => {
            let mut metrics = Vec::new();
            for intent in measures {
                match intent {
                    AggIntent::Quantile { .. } | AggIntent::HistogramQuantile { .. } => {
                        return None
                    }
                    intent => {
                        metrics.push(intent_metric(intent).unwrap_or(AccuracyMetric::ABSOLUTE))
                    }
                }
            }
            // Measures with different default metrics share one value-unit
            // statement.
            Some(match metrics.split_first() {
                Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                _ => AccuracyMetric::ABSOLUTE,
            })
        }
        _ => Some(AccuracyMetric::ABSOLUTE),
    }
}

/// The metric of `node`'s output `column`, following columns through
/// projections, `CASE` value branches, joins and operators that only
/// select, order or rename rows, down to an aggregate's measure. A group
/// key or any other expression is absolute.
fn column_metric(node: &OperatorNode, column: usize) -> Option<AccuracyMetric> {
    use crate::ir::operator::operator_properties::JoinKind;
    let Operator::NonASAP(op) = &node.operator else {
        return Some(AccuracyMetric::ABSOLUTE);
    };
    match op {
        NonASAPOp::Filter { child, .. }
        | NonASAPOp::Sort { child, .. }
        | NonASAPOp::Limit { child, .. }
        | NonASAPOp::Dedup { child, .. } => column_metric(child, column),
        NonASAPOp::Project { cols, child, .. } => match cols.get(column).map(|item| &item.expr) {
            Some(ScalarExpr::Column(id)) => column_metric(child, *id),
            Some(ScalarExpr::Case {
                branches,
                else_expr,
                ..
            }) => {
                let mut metrics = Vec::new();
                for value in branches
                    .iter()
                    .map(|(_, then)| then)
                    .chain(else_expr.as_deref())
                {
                    match value {
                        // A literal, possibly cast (SQL's typed NULL).
                        ScalarExpr::Literal(_) => {}
                        ScalarExpr::Cast { expr, .. }
                            if matches!(**expr, ScalarExpr::Literal(_)) => {}
                        ScalarExpr::Column(id) => metrics.push(column_metric(child, *id)?),
                        _ => return Some(AccuracyMetric::ABSOLUTE),
                    }
                }
                Some(match metrics.split_first() {
                    Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                    _ => AccuracyMetric::ABSOLUTE,
                })
            }
            _ => Some(AccuracyMetric::ABSOLUTE),
        },
        NonASAPOp::Join {
            kind:
                JoinKind::Inner | JoinKind::Left | JoinKind::Right | JoinKind::Full | JoinKind::Cross,
            left,
            right,
            ..
        } => {
            let width = left.schema.fields.len();
            if column < width {
                column_metric(left, column)
            } else {
                column_metric(right, column - width)
            }
        }
        NonASAPOp::Aggregate {
            reduction,
            measures,
            child,
            ..
        } => match aggregate_measure_at(node, reduction, measures, child, column) {
            Some(AggIntent::Quantile { .. } | AggIntent::HistogramQuantile { .. }) => None,
            Some(intent) => Some(intent_metric(intent).unwrap_or(AccuracyMetric::ABSOLUTE)),
            None => Some(AccuracyMetric::ABSOLUTE),
        },
        _ => Some(AccuracyMetric::ABSOLUTE),
    }
}

/// The measure the aggregate `node`'s output `column` holds, or `None` for
/// a group key, a kept label or `count_values`' synthesized label, as
/// [`aggregate_output_schema`](crate::ir::schema::aggregate_schema::aggregate_output_schema)
/// lays the columns out. A per-entity reduction replaces its one measure's
/// input column in place and keeps every other column; a cross-series one
/// emits its kept labels first, then one column per measure, with a
/// `count_values` label (unless an earlier column has its name) before its
/// count.
fn aggregate_measure_at<'a>(
    node: &OperatorNode,
    reduction: &crate::ir::operator::operator_properties::Reduction,
    measures: &'a [AggIntent],
    child: &OperatorNode,
    column: usize,
) -> Option<&'a AggIntent> {
    use crate::ir::operator::operator_properties::Reduction;
    use crate::ir::scalar::{resolve_column_ref, ColumnRef};
    let fields = &node.schema.fields;
    match reduction {
        Reduction::PerEntity => {
            let [measure] = measures else { return None };
            let value = match measure.input_cols().first() {
                Some(input) => *input,
                None => resolve_column_ref(&ColumnRef::SampleValue, &child.schema).ok()?,
            };
            (column == value).then_some(measure)
        }
        Reduction::Reduce(by) => {
            let mut next = if by.is_without() {
                fields.len().checked_sub(measures.len())?
            } else {
                by.len()
            };
            for measure in measures {
                if let AggIntent::CountValues { label } = measure {
                    if !by.is_without()
                        && !fields[..next.min(fields.len())]
                            .iter()
                            .any(|f| f.name == *label)
                    {
                        next += 1;
                    }
                }
                if next == column {
                    return Some(measure);
                }
                next += 1;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::operator::{Reduction, Source};
    use crate::ir::schema::{DataType, Field, Schema};
    use std::rc::Rc;

    fn node(op: NonASAPOp) -> Rc<OperatorNode> {
        Rc::new(OperatorNode::new(Operator::NonASAP(op)).expect("fixture derives a schema"))
    }

    fn aggregate(measures: Vec<AggIntent>) -> QueryRoot {
        let scan = node(NonASAPOp::Scan {
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
        });
        QueryRoot::Operator(node(NonASAPOp::Aggregate {
            reduction: Reduction::by(vec![]),
            measures,
            output_names: vec![],
            filters: vec![],
            having: None,
            child: scan,
        }))
    }

    fn quantile() -> AggIntent {
        AggIntent::Quantile {
            col: None,
            q: 0.99,
            accuracy: AccuracyTarget::max_error(0.01),
        }
    }

    /// A target reads and writes as a plain struct: every field but
    /// `max_error` may be omitted (no metric, no δ, the whole result), an unset
    /// metric or δ writes nothing, and the old named forms (`"Exact"`,
    /// `{"Epsilon": ε}`, `{"EpsilonDelta": …}`) and unknown fields are
    /// rejected.
    #[test]
    fn accuracy_target_json_is_a_plain_struct() {
        use serde_json::json;
        let read = |value: serde_json::Value| serde_json::from_value::<AccuracyTarget>(value);
        assert_eq!(
            read(json!({"max_error": 0.01})).unwrap(),
            AccuracyTarget::max_error(0.01)
        );
        assert_eq!(
            read(json!({"max_error": 0.0, "failure_probability": 0.0})).unwrap(),
            AccuracyTarget::exact()
        );
        let rank = AccuracyTarget::max_error(0.01)
            .failure_probability(0.001)
            .accuracy_metric(AccuracyMetric::Rank);
        assert_eq!(
            read(
                json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.001})
            )
            .unwrap(),
            rank
        );
        assert_eq!(
            serde_json::to_value(&rank).unwrap(),
            json!({"accuracy_metric": "rank", "max_error": 0.01, "failure_probability": 0.001,
                   "accuracy_scope": "whole_result"})
        );
        assert_eq!(
            serde_json::to_value(AccuracyTarget::max_error(0.01)).unwrap(),
            json!({"max_error": 0.01, "accuracy_scope": "whole_result"})
        );
        for old in [
            json!("Exact"),
            json!({"Epsilon": 0.01}),
            json!({"EpsilonDelta": {"epsilon": 0.01, "delta": 0.001}}),
            json!({"max_error": 0.1, "typo": true}),
        ] {
            assert!(read(old.clone()).is_err(), "{old}");
        }
    }

    /// Metrics have stable wire names.
    #[test]
    fn metric_wire_names() {
        for (metric, name) in [
            (AccuracyMetric::ABSOLUTE, "absolute"),
            (AccuracyMetric::ABSOLUTE_STREAM_L1, "absolute_stream_l1"),
            (AccuracyMetric::ABSOLUTE_STREAM_L2, "absolute_stream_l2"),
            (AccuracyMetric::Relative, "relative"),
            (AccuracyMetric::Rank, "rank"),
        ] {
            assert_eq!(serde_json::to_value(metric).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<AccuracyMetric>(name.into()).unwrap(),
                metric
            );
        }
    }

    /// A per-entity count (`count_over_time`) keeps every label column and
    /// replaces the sample value in place: the value column carries the
    /// count's metric, and a label after it is absolute.
    #[test]
    fn a_per_entity_count_states_its_value_column_not_its_last_column() {
        let scan = node(NonASAPOp::Scan {
            source: Source::TimeSeries { metric: "m".into() },
            predicates: vec![],
            schema: Schema::with_time_index(
                vec![
                    Field::plain("ts", DataType::Timestamp, false),
                    Field::plain("value", DataType::Float64, false),
                    Field::plain("job", DataType::Utf8, false),
                ],
                0,
                vec![],
            ),
        });
        let count = node(NonASAPOp::Aggregate {
            reduction: Reduction::PerEntity,
            measures: vec![AggIntent::Count {
                accuracy: AccuracyTarget::max_error(0.01),
            }],
            output_names: vec![],
            filters: vec![],
            having: None,
            child: scan,
        });
        assert_eq!(count.schema.fields[1].name, "value");
        assert_eq!(
            column_metric(&count, 1),
            Some(AccuracyMetric::ABSOLUTE_STREAM_L1)
        );
        assert_eq!(column_metric(&count, 2), Some(AccuracyMetric::ABSOLUTE));
    }

    /// A root's default metric follows its computation: counts and top-k
    /// frequencies are L1-scaled absolute errors, distinct counts and L2
    /// frequency moments relative ones, anything else (a sum or an entropy
    /// among them) absolute value errors.
    #[test]
    fn root_metric_defaults_follow_the_query_intent() {
        let metric = |intent: AggIntent| default_metric_for_root(&aggregate(vec![intent])).unwrap();
        let exact = AccuracyTarget::exact();
        assert_eq!(
            metric(AggIntent::Count {
                accuracy: exact.clone()
            }),
            AccuracyMetric::ABSOLUTE_STREAM_L1
        );
        assert_eq!(
            metric(AggIntent::Sum { col: None }),
            AccuracyMetric::ABSOLUTE
        );
        assert_eq!(
            metric(AggIntent::TopK {
                k: 5,
                accuracy: exact.clone()
            }),
            AccuracyMetric::ABSOLUTE_STREAM_L1
        );
        assert_eq!(
            metric(AggIntent::Cardinality {
                cols: vec![],
                accuracy: exact.clone()
            }),
            AccuracyMetric::Relative
        );
        assert_eq!(
            metric(AggIntent::FrequencyL2 {
                col: None,
                accuracy: exact.clone()
            }),
            AccuracyMetric::Relative
        );
        assert_eq!(
            metric(AggIntent::FrequencyEntropy {
                col: None,
                accuracy: exact.clone()
            }),
            AccuracyMetric::ABSOLUTE
        );
        assert_eq!(
            metric(AggIntent::Avg { col: None }),
            AccuracyMetric::ABSOLUTE
        );
        assert_eq!(
            default_metric_for_root(&QueryRoot::Scalar(ScalarExpr::literal_f64(1.0))),
            Some(AccuracyMetric::ABSOLUTE)
        );
    }

    /// SQL's empty-input NULL guard (a statistic cross-joined with an exact
    /// row count, and `CASE WHEN count = 0 THEN NULL ELSE statistic END`)
    /// keeps the statistic's metric: the root's value is the statistic's
    /// column. A `CASE` whose value branches read columns of different
    /// metrics, or any other expression, is absolute.
    #[test]
    fn root_metric_looks_through_a_null_guard_to_its_statistic() {
        use crate::ir::operator::operator_properties::JoinKind;
        use crate::ir::scalar::{CompareOpKind, ScalarValue};
        use crate::ir::{Predicate, ProjectItem};
        let exact = AccuracyTarget::exact();
        let QueryRoot::Operator(l2) = aggregate(vec![AggIntent::FrequencyL2 {
            col: None,
            accuracy: exact.clone(),
        }]) else {
            unreachable!()
        };
        let QueryRoot::Operator(count) = aggregate(vec![AggIntent::Count { accuracy: exact }])
        else {
            unreachable!()
        };
        let joined = node(NonASAPOp::Join {
            kind: JoinKind::Cross,
            pred: Predicate(ScalarExpr::Literal(ScalarValue::Boolean(true))),
            left: l2,
            right: count,
        });
        let guard = |otherwise: ScalarExpr| {
            QueryRoot::Operator(node(NonASAPOp::Project {
                cols: vec![ProjectItem {
                    alias: Some("v".into()),
                    expr: ScalarExpr::Case {
                        operand: None,
                        branches: vec![(
                            ScalarExpr::Compare {
                                left: Box::new(ScalarExpr::Column(1)),
                                op: CompareOpKind::Eq,
                                right: Box::new(ScalarExpr::Literal(ScalarValue::Int64(0))),
                                semantics: crate::ir::ExprSemantics::Sql,
                            },
                            ScalarExpr::Cast {
                                expr: Box::new(ScalarExpr::Literal(ScalarValue::Null)),
                                to: DataType::Float64,
                                try_cast: false,
                            },
                        )],
                        else_expr: Some(Box::new(otherwise)),
                    },
                }],
                qualifier: None,
                child: joined.clone(),
            }))
        };
        let metric = |root: QueryRoot| default_metric_for_root(&root).unwrap();
        assert_eq!(
            metric(guard(ScalarExpr::Column(0))),
            AccuracyMetric::Relative
        );
        // The count's column, or arithmetic over the statistic.
        assert_eq!(
            metric(guard(ScalarExpr::Column(1))),
            AccuracyMetric::ABSOLUTE_STREAM_L1
        );
        assert_eq!(
            metric(guard(ScalarExpr::Negative {
                expr: Box::new(ScalarExpr::Column(0)),
                semantics: crate::ir::ExprSemantics::Sql,
            })),
            AccuracyMetric::ABSOLUTE
        );
    }

    /// A quantile root has no default metric: its error is as naturally a
    /// rank as a relative one, and a target that sets none accepts either.
    /// It is not an input error.
    #[test]
    fn a_quantile_root_has_no_default_metric() {
        assert_eq!(default_metric_for_root(&aggregate(vec![quantile()])), None);
    }

    /// A target on the groups holding at least a share of the records reads
    /// and writes its share as a fraction; a share outside (0, 1] (say 5
    /// meant as 5%, or 0, which would cover every group) is bad input.
    #[test]
    fn a_record_share_scope_reads_its_share_as_a_fraction() {
        use serde_json::json;
        let target = AccuracyTarget::max_error(0.05)
            .failure_probability(0.05)
            .accuracy_scope(AccuracyScope::GroupsWithRecordShareAtLeast(0.05));
        let wire = json!({"max_error": 0.05, "failure_probability": 0.05,
                          "accuracy_scope": {"groups_with_record_share_at_least": 0.05}});
        assert_eq!(serde_json::to_value(&target).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<AccuracyTarget>(wire).unwrap(),
            target
        );
        for share in [json!(0.0), json!(-0.1), json!(5.0), json!(1.5)] {
            let bad = json!({"max_error": 0.05,
                             "accuracy_scope": {"groups_with_record_share_at_least": share}});
            assert!(
                serde_json::from_value::<AccuracyTarget>(bad).is_err(),
                "{share}"
            );
        }
    }

    /// How many values one statement covers at most: one for a per-value
    /// scope, the `G` values of the result for the whole result (unbound when
    /// `G` is), and at most `⌊1 / share⌋` groups for a record share, since
    /// more groups cannot each hold that share. So a 1% scope covers 100,
    /// a 5% scope 20, fewer when the result has fewer values.
    #[test]
    fn a_scope_covers_at_most_one_over_its_share_groups() {
        use AccuracyScope::*;
        assert_eq!(PerValue.max_values_covered(None), Some(1));
        assert_eq!(WholeResult.max_values_covered(None), None);
        assert_eq!(WholeResult.max_values_covered(Some(625)), Some(625));
        assert_eq!(
            GroupsWithRecordShareAtLeast(0.05).max_values_covered(None),
            Some(20)
        );
        assert_eq!(
            GroupsWithRecordShareAtLeast(0.01).max_values_covered(None),
            Some(100)
        );
        assert_eq!(
            GroupsWithRecordShareAtLeast(0.03).max_values_covered(None),
            Some(33)
        );
        assert_eq!(
            GroupsWithRecordShareAtLeast(1.0).max_values_covered(None),
            Some(1)
        );
        for invalid in [0.0, -0.1, 1.5, f64::NAN] {
            assert_eq!(
                GroupsWithRecordShareAtLeast(invalid).max_values_covered(None),
                None,
                "{invalid}"
            );
        }
        assert_eq!(
            GroupsWithRecordShareAtLeast(0.01).max_values_covered(Some(4)),
            Some(4)
        );
    }

    /// The whole result covers every scope; a record share covers a larger
    /// share but not a single value, which may be a smaller group; a single
    /// value covers only itself.
    #[test]
    fn a_wider_scope_covers_a_narrower_one() {
        use AccuracyScope::*;
        let share = GroupsWithRecordShareAtLeast;
        assert!(WholeResult.covers(share(0.01)));
        assert!(share(0.01).covers(share(0.05)));
        assert!(!share(0.05).covers(PerValue));
        assert!(PerValue.covers(PerValue));
        assert!(!share(0.05).covers(share(0.01)));
        assert!(!share(0.05).covers(WholeResult));
        assert!(!PerValue.covers(share(0.05)));
    }

    /// Exact means zero error with certainty, whatever the metric; an
    /// ε of zero without a δ is not exact.
    #[test]
    fn exact_targets() {
        assert!(AccuracyTarget::exact().is_exact());
        assert_eq!(AccuracyTarget::exact().accuracy_metric, None);
        assert!(AccuracyTarget::exact()
            .accuracy_metric(AccuracyMetric::Rank)
            .is_exact());
        assert!(!AccuracyTarget::max_error(0.0).is_exact());
        assert_eq!(
            AccuracyTarget::max_error(0.01)
                .failure_probability(0.001)
                .failure_probability,
            Some(0.001)
        );
        assert_eq!(
            AccuracyTarget::max_error(0.01).accuracy_scope,
            AccuracyScope::WholeResult
        );
    }
}
