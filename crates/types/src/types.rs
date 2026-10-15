//! Accuracy requirements and the error metrics they are stated in (#690).
//!
//! A requirement and a guarantee share one outer shape, [`AccuracySpec`]:
//! an error metric, an error ε, a failure probability δ and a scope. A
//! requirement ([`AccuracyTarget`]) holds concrete numbers and may leave its
//! metric unset; a guarantee ([`crate::post_asap::AccuracyGuarantee`])
//! always states its metric and holds symbolic expressions that plan
//! selection binds. A target without a metric accepts a guarantee in any
//! metric that meets its ε, δ and scope.

use serde::{Deserialize, Serialize};

use crate::ir::{NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};
use crate::pre_asap::AggIntent;

/// The stream norm an [`ErrorMetric::Absolute`] error is stated in
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
pub enum ErrorMetric {
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

impl ErrorMetric {
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

/// The wire name of an [`ErrorMetric`].
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MetricName {
    Absolute,
    AbsoluteStreamL1,
    AbsoluteStreamL2,
    Relative,
    Rank,
}

impl From<ErrorMetric> for MetricName {
    fn from(metric: ErrorMetric) -> Self {
        match metric {
            ErrorMetric::Absolute { scale: None } => Self::Absolute,
            ErrorMetric::Absolute {
                scale: Some(NormScale::StreamL1),
            } => Self::AbsoluteStreamL1,
            ErrorMetric::Absolute {
                scale: Some(NormScale::StreamL2),
            } => Self::AbsoluteStreamL2,
            ErrorMetric::Relative => Self::Relative,
            ErrorMetric::Rank => Self::Rank,
        }
    }
}

impl From<MetricName> for ErrorMetric {
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
}

/// The outer shape of an accuracy guarantee
/// ([`crate::post_asap::AccuracyGuarantee`]):
/// `Pr[err_metric(estimate, truth) > error] ≤ failure_probability` over the
/// values `scope` names. An [`AccuracyTarget`] has the same fields, with
/// concrete numbers and an optional metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracySpec<E, P> {
    pub metric: ErrorMetric,
    /// ε, in `metric`'s units.
    pub error: E,
    /// δ.
    pub failure_probability: P,
    pub scope: AccuracyScope,
}

/// An accuracy requirement, as a workload, a caller or a frontend states
/// it: `Pr[err_metric(estimate, truth) > error] ≤ failure_probability` over
/// the values `scope` names.
///
/// `metric: None` means the user did not set one: plan selection then
/// accepts a guarantee in any metric whose ε, δ and scope meet this
/// target's, and reports the metric it accepted. A set metric accepts only
/// guarantees in that metric (or convertible to it with evidence).
/// `failure_probability: None` imposes no δ requirement (sizing then uses
/// the planner's default δ). Exact is `error = 0`,
/// `failure_probability = Some(0)`, whatever the metric.
///
/// JSON: `{"metric": "rank", "error": 0.01, "failure_probability": 0.001,
/// "scope": "whole_result"}`; every field but `error` may be omitted
/// (`scope` defaults to `whole_result`). Exact is
/// `{"error": 0.0, "failure_probability": 0.0}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccuracyTarget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<ErrorMetric>,
    /// ε, in the metric's units.
    pub error: f64,
    /// δ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_probability: Option<f64>,
    #[serde(default = "whole_result")]
    pub scope: AccuracyScope,
}

fn whole_result() -> AccuracyScope {
    AccuracyScope::WholeResult
}

impl AccuracyTarget {
    /// No approximation permitted.
    pub const fn exact() -> Self {
        Self {
            metric: None,
            error: 0.0,
            failure_probability: Some(0.0),
            scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `epsilon`, with no δ requirement and no metric set.
    /// [`Self::with_metric`] sets one.
    pub const fn epsilon(epsilon: f64) -> Self {
        Self {
            metric: None,
            error: epsilon,
            failure_probability: None,
            scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `epsilon` with probability at least `1 − delta`, and
    /// no metric set. [`Self::with_metric`] sets one.
    pub const fn epsilon_delta(epsilon: f64, delta: f64) -> Self {
        Self {
            failure_probability: Some(delta),
            ..Self::epsilon(epsilon)
        }
    }

    /// This target with its metric set to `metric`.
    pub fn with_metric(self, metric: ErrorMetric) -> Self {
        Self {
            metric: Some(metric),
            ..self
        }
    }

    /// This target over `scope`.
    pub fn with_scope(self, scope: AccuracyScope) -> Self {
        Self { scope, ..self }
    }

    /// Whether this target permits no approximation.
    pub fn is_exact(&self) -> bool {
        self.error == 0.0 && self.failure_probability == Some(0.0)
    }

    /// This target's ε, δ and scope, stated in `metric`: the target an
    /// aggregate intent's summary is sized against, in the intent's own
    /// metric. An exact target stays [`Self::exact`]: exact is exact in
    /// every metric.
    pub fn target_in(&self, metric: ErrorMetric) -> Self {
        if self.is_exact() {
            return Self::exact().with_scope(self.scope);
        }
        self.clone().with_metric(metric)
    }

    /// The target a quantile intent is sized against: the set metric, or
    /// `Rank` when none is set. A summary is sized in its own native terms
    /// (#690 §4), so only ε, δ and the scope matter there; the query's own
    /// metric, set or not, is checked at its root.
    ///
    /// The other intents with a target are stated in their own metric with
    /// [`Self::target_in`]: an L1-scaled absolute error for counts and top-k
    /// frequencies, a relative error for distinct counts, and an absolute
    /// value error for frequency moments.
    pub fn for_quantile(&self) -> Self {
        self.target_in(self.metric.unwrap_or(ErrorMetric::Rank))
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
pub fn default_metric_for_root(root: &QueryRoot) -> Option<ErrorMetric> {
    match root {
        QueryRoot::Operator(node) => node_metric(node),
        QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node))
        | QueryRoot::Scalar(ScalarExpr::ScalarSubquery(node)) => node_metric(node),
        QueryRoot::Scalar(_) => Some(ErrorMetric::ABSOLUTE),
    }
}

/// The default metric of an intent's value, `None` for a quantile and for
/// intents without a listed default.
fn intent_metric(intent: &AggIntent) -> Option<ErrorMetric> {
    match intent {
        AggIntent::Count { .. } | AggIntent::TopK { .. } => Some(ErrorMetric::ABSOLUTE_STREAM_L1),
        AggIntent::Cardinality { .. } | AggIntent::FrequencyL2 { .. } => {
            Some(ErrorMetric::Relative)
        }
        _ => None,
    }
}

/// The metric of `node`'s values, looking through operators that only
/// select, order or rename rows.
fn node_metric(node: &OperatorNode) -> Option<ErrorMetric> {
    let Operator::NonASAP(op) = &node.operator else {
        return Some(ErrorMetric::ABSOLUTE);
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
                _ => ErrorMetric::ABSOLUTE,
            }),
        NonASAPOp::Aggregate { measures, .. } => {
            let mut metrics = Vec::new();
            for intent in measures {
                match intent {
                    AggIntent::Quantile { .. } | AggIntent::HistogramQuantile { .. } => {
                        return None
                    }
                    intent => metrics.push(intent_metric(intent).unwrap_or(ErrorMetric::ABSOLUTE)),
                }
            }
            // Measures with different default metrics share one value-unit
            // statement.
            Some(match metrics.split_first() {
                Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                _ => ErrorMetric::ABSOLUTE,
            })
        }
        _ => Some(ErrorMetric::ABSOLUTE),
    }
}

/// The metric of `node`'s output `column`, following columns through
/// projections, `CASE` value branches, joins and operators that only
/// select, order or rename rows, down to an aggregate's measure. A group
/// key or any other expression is absolute.
fn column_metric(node: &OperatorNode, column: usize) -> Option<ErrorMetric> {
    use crate::ir::operator_properties::JoinKind;
    let Operator::NonASAP(op) = &node.operator else {
        return Some(ErrorMetric::ABSOLUTE);
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
                        _ => return Some(ErrorMetric::ABSOLUTE),
                    }
                }
                Some(match metrics.split_first() {
                    Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                    _ => ErrorMetric::ABSOLUTE,
                })
            }
            _ => Some(ErrorMetric::ABSOLUTE),
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
        NonASAPOp::Aggregate { measures, .. } => {
            // Group keys first, then one column per measure.
            let keys = node.schema.fields.len().saturating_sub(measures.len());
            match column.checked_sub(keys).and_then(|m| measures.get(m)) {
                Some(AggIntent::Quantile { .. } | AggIntent::HistogramQuantile { .. }) => None,
                Some(intent) => Some(intent_metric(intent).unwrap_or(ErrorMetric::ABSOLUTE)),
                None => Some(ErrorMetric::ABSOLUTE),
            }
        }
        _ => Some(ErrorMetric::ABSOLUTE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pre_asap::schema::{DataType, Field, Schema};
    use crate::pre_asap::{Reduction, Source};
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
            accuracy: AccuracyTarget::epsilon(0.01),
        }
    }

    /// A target reads and writes as a plain struct: every field but
    /// `error` may be omitted (no metric, no δ, the whole result), an unset
    /// metric or δ writes nothing, and the old named forms (`"Exact"`,
    /// `{"Epsilon": ε}`, `{"EpsilonDelta": …}`) and unknown fields are
    /// rejected.
    #[test]
    fn accuracy_target_json_is_a_plain_struct() {
        use serde_json::json;
        let read = |value: serde_json::Value| serde_json::from_value::<AccuracyTarget>(value);
        assert_eq!(
            read(json!({"error": 0.01})).unwrap(),
            AccuracyTarget::epsilon(0.01)
        );
        assert_eq!(
            read(json!({"error": 0.0, "failure_probability": 0.0})).unwrap(),
            AccuracyTarget::exact()
        );
        let rank = AccuracyTarget::epsilon_delta(0.01, 0.001).with_metric(ErrorMetric::Rank);
        assert_eq!(
            read(json!({"metric": "rank", "error": 0.01, "failure_probability": 0.001})).unwrap(),
            rank
        );
        assert_eq!(
            serde_json::to_value(&rank).unwrap(),
            json!({"metric": "rank", "error": 0.01, "failure_probability": 0.001,
                   "scope": "whole_result"})
        );
        assert_eq!(
            serde_json::to_value(AccuracyTarget::epsilon(0.01)).unwrap(),
            json!({"error": 0.01, "scope": "whole_result"})
        );
        for old in [
            json!("Exact"),
            json!({"Epsilon": 0.01}),
            json!({"EpsilonDelta": {"epsilon": 0.01, "delta": 0.001}}),
            json!({"error": 0.1, "typo": true}),
        ] {
            assert!(read(old.clone()).is_err(), "{old}");
        }
    }

    /// Metrics have stable wire names.
    #[test]
    fn metric_wire_names() {
        for (metric, name) in [
            (ErrorMetric::ABSOLUTE, "absolute"),
            (ErrorMetric::ABSOLUTE_STREAM_L1, "absolute_stream_l1"),
            (ErrorMetric::ABSOLUTE_STREAM_L2, "absolute_stream_l2"),
            (ErrorMetric::Relative, "relative"),
            (ErrorMetric::Rank, "rank"),
        ] {
            assert_eq!(serde_json::to_value(metric).unwrap(), name);
            assert_eq!(
                serde_json::from_value::<ErrorMetric>(name.into()).unwrap(),
                metric
            );
        }
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
            ErrorMetric::ABSOLUTE_STREAM_L1
        );
        assert_eq!(metric(AggIntent::Sum { col: None }), ErrorMetric::ABSOLUTE);
        assert_eq!(
            metric(AggIntent::TopK {
                k: 5,
                accuracy: exact.clone()
            }),
            ErrorMetric::ABSOLUTE_STREAM_L1
        );
        assert_eq!(
            metric(AggIntent::Cardinality {
                cols: vec![],
                accuracy: exact.clone()
            }),
            ErrorMetric::Relative
        );
        assert_eq!(
            metric(AggIntent::FrequencyL2 {
                col: None,
                accuracy: exact.clone()
            }),
            ErrorMetric::Relative
        );
        assert_eq!(
            metric(AggIntent::FrequencyEntropy {
                col: None,
                accuracy: exact.clone()
            }),
            ErrorMetric::ABSOLUTE
        );
        assert_eq!(metric(AggIntent::Avg { col: None }), ErrorMetric::ABSOLUTE);
        assert_eq!(
            default_metric_for_root(&QueryRoot::Scalar(ScalarExpr::literal_f64(1.0))),
            Some(ErrorMetric::ABSOLUTE)
        );
    }

    /// SQL's empty-input NULL guard (a statistic cross-joined with an exact
    /// row count, and `CASE WHEN count = 0 THEN NULL ELSE statistic END`)
    /// keeps the statistic's metric: the root's value is the statistic's
    /// column. A `CASE` whose value branches read columns of different
    /// metrics, or any other expression, is absolute.
    #[test]
    fn root_metric_looks_through_a_null_guard_to_its_statistic() {
        use crate::ir::operator_properties::JoinKind;
        use crate::ir::{Predicate, ProjectItem};
        use crate::pre_asap::{CompareOpKind, ScalarValue};
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
        assert_eq!(metric(guard(ScalarExpr::Column(0))), ErrorMetric::Relative);
        // The count's column, or arithmetic over the statistic.
        assert_eq!(
            metric(guard(ScalarExpr::Column(1))),
            ErrorMetric::ABSOLUTE_STREAM_L1
        );
        assert_eq!(
            metric(guard(ScalarExpr::Negative {
                expr: Box::new(ScalarExpr::Column(0)),
                semantics: crate::ir::ExprSemantics::Sql,
            })),
            ErrorMetric::ABSOLUTE
        );
    }

    /// A quantile root has no default metric: its error is as naturally a
    /// rank as a relative one, and a target that sets none accepts either.
    /// It is not an input error.
    #[test]
    fn a_quantile_root_has_no_default_metric() {
        assert_eq!(default_metric_for_root(&aggregate(vec![quantile()])), None);
    }

    /// Exact means zero error with certainty, whatever the metric; an
    /// ε of zero without a δ is not exact.
    #[test]
    fn exact_targets() {
        assert!(AccuracyTarget::exact().is_exact());
        assert_eq!(AccuracyTarget::exact().metric, None);
        assert!(AccuracyTarget::exact()
            .with_metric(ErrorMetric::Rank)
            .is_exact());
        assert!(!AccuracyTarget::epsilon(0.0).is_exact());
        assert_eq!(
            AccuracyTarget::epsilon_delta(0.01, 0.001).failure_probability,
            Some(0.001)
        );
        assert_eq!(
            AccuracyTarget::epsilon(0.01).scope,
            AccuracyScope::WholeResult
        );
    }
}
