//! Accuracy requirements and the error metrics they are stated in (#690).
//!
//! A requirement and a guarantee share one outer shape, [`AccuracySpec`]:
//! an error metric, an error ε, a failure probability δ and a scope. A
//! requirement ([`AccuracyTarget`]) holds concrete numbers; a guarantee
//! ([`crate::post_asap::AccuracyGuarantee`]) holds symbolic expressions that
//! plan selection binds. Workloads and the query-language frontends take a
//! [`RequestedAccuracy`], whose metric may be omitted; it is resolved to an
//! [`AccuracyTarget`] for each aggregate intent and for each query root.

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

/// The outer shape an accuracy target and an accuracy guarantee share:
/// `Pr[err_metric(estimate, truth) > error] ≤ failure_probability` over the
/// values `scope` names.
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

/// A concrete accuracy requirement. `failure_probability: None` imposes no
/// δ requirement (sizing then uses the planner's default δ). Exact is
/// `error = 0`, `failure_probability = Some(0)`, whatever the metric.
pub type AccuracyTarget = AccuracySpec<f64, Option<f64>>;

impl AccuracySpec<f64, Option<f64>> {
    /// No approximation permitted.
    pub const fn exact() -> Self {
        Self {
            metric: ErrorMetric::ABSOLUTE,
            error: 0.0,
            failure_probability: Some(0.0),
            scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `epsilon` in absolute value units, with no δ
    /// requirement. [`Self::with_metric`] states another metric.
    pub const fn epsilon(epsilon: f64) -> Self {
        Self {
            metric: ErrorMetric::ABSOLUTE,
            error: epsilon,
            failure_probability: None,
            scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `epsilon` in absolute value units with probability at
    /// least `1 − delta`. [`Self::with_metric`] states another metric.
    pub const fn epsilon_delta(epsilon: f64, delta: f64) -> Self {
        Self {
            failure_probability: Some(delta),
            ..Self::epsilon(epsilon)
        }
    }

    /// This target stated in `metric`.
    pub fn with_metric(self, metric: ErrorMetric) -> Self {
        Self { metric, ..self }
    }

    /// This target over `scope`.
    pub fn with_scope(self, scope: AccuracyScope) -> Self {
        Self { scope, ..self }
    }

    /// Whether this target permits no approximation.
    pub fn is_exact(&self) -> bool {
        self.error == 0.0 && self.failure_probability == Some(0.0)
    }
}

/// An accuracy requirement as a workload or a caller states it: the
/// metric may be omitted. [`Self::for_root`] resolves it for a query root
/// (the target plan selection checks); [`Self::target_in`] states it in an
/// aggregate intent's own metric (the target a summary is sized against).
///
/// Reads the workload forms `"Exact"`, `{"Epsilon": ε}` and
/// `{"EpsilonDelta": {"epsilon": ε, "delta": δ}}`, as well as
/// `{"metric": …, "error": ε, "failure_probability": δ, "scope": …}`, in
/// which every field but `error` is optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "RequestedAccuracyWire")]
pub struct RequestedAccuracy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<ErrorMetric>,
    pub error: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_probability: Option<f64>,
    pub scope: AccuracyScope,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RequestedAccuracyWire {
    Named(NamedTarget),
    Spec(SpecWire),
}

/// The workload forms that name no metric.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum NamedTarget {
    Epsilon(f64),
    EpsilonDelta { epsilon: f64, delta: f64 },
    Exact,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpecWire {
    #[serde(default)]
    metric: Option<ErrorMetric>,
    error: f64,
    #[serde(default)]
    failure_probability: Option<f64>,
    #[serde(default = "whole_result")]
    scope: AccuracyScope,
}

fn whole_result() -> AccuracyScope {
    AccuracyScope::WholeResult
}

impl From<RequestedAccuracyWire> for RequestedAccuracy {
    fn from(wire: RequestedAccuracyWire) -> Self {
        match wire {
            RequestedAccuracyWire::Named(NamedTarget::Exact) => Self::exact(),
            RequestedAccuracyWire::Named(NamedTarget::Epsilon(epsilon)) => Self::epsilon(epsilon),
            RequestedAccuracyWire::Named(NamedTarget::EpsilonDelta { epsilon, delta }) => {
                Self::epsilon_delta(epsilon, delta)
            }
            RequestedAccuracyWire::Spec(spec) => Self {
                metric: spec.metric,
                error: spec.error,
                failure_probability: spec.failure_probability,
                scope: spec.scope,
            },
        }
    }
}

/// A requested accuracy that cannot be resolved for a query root.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum AccuracyRequestError {
    /// A quantile's error may be a rank or a value error, and which one is
    /// the user's choice: the planner never guesses it.
    #[error(
        "the accuracy target of a {intent} query must name its metric \
         (\"rank\", \"relative\" or \"absolute\")"
    )]
    MissingQuantileMetric { intent: &'static str },
}

impl RequestedAccuracy {
    /// No approximation permitted.
    pub const fn exact() -> Self {
        Self {
            metric: None,
            error: 0.0,
            failure_probability: Some(0.0),
            scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `epsilon` in the query's default metric, with no δ
    /// requirement.
    pub const fn epsilon(epsilon: f64) -> Self {
        Self {
            metric: None,
            error: epsilon,
            failure_probability: None,
            scope: AccuracyScope::WholeResult,
        }
    }

    /// Error at most `epsilon` in the query's default metric with
    /// probability at least `1 − delta`.
    pub const fn epsilon_delta(epsilon: f64, delta: f64) -> Self {
        Self {
            failure_probability: Some(delta),
            ..Self::epsilon(epsilon)
        }
    }

    /// This request stated in `metric`.
    pub fn with_metric(self, metric: ErrorMetric) -> Self {
        Self {
            metric: Some(metric),
            ..self
        }
    }

    /// Whether this request permits no approximation.
    pub fn is_exact(&self) -> bool {
        self.error == 0.0 && self.failure_probability == Some(0.0)
    }

    /// This request's ε, δ and scope, stated in `metric`. An exact request
    /// is [`AccuracyTarget::exact`] whatever `metric`: exact is exact in
    /// every metric.
    pub fn target_in(&self, metric: ErrorMetric) -> AccuracyTarget {
        if self.is_exact() {
            return AccuracyTarget::exact().with_scope(self.scope);
        }
        AccuracyTarget {
            metric,
            error: self.error,
            failure_probability: self.failure_probability,
            scope: self.scope,
        }
    }

    /// The target a quantile intent is sized against: the requested metric,
    /// or `Rank` when none is named. A summary is sized in its own native
    /// terms (#690 §4), so only ε, δ and the scope matter there; a quantile
    /// root's metric must be named ([`Self::for_root`]).
    ///
    /// The other intents with a target are stated in their own metric with
    /// [`Self::target_in`]: an L1-scaled absolute error for counts and top-k
    /// frequencies, a relative error for distinct counts, and an absolute
    /// value error for frequency moments. The query's own metric is checked
    /// at its root.
    pub fn for_quantile(&self) -> AccuracyTarget {
        self.target_in(self.metric.unwrap_or(ErrorMetric::Rank))
    }

    /// The target plan selection checks at `root`: the requested metric, or
    /// the one derived from the root's computation — an L1-scaled absolute
    /// error for counts and top-k frequencies, a relative error for
    /// distinct counts, and an absolute value error for anything else
    /// (sums, arithmetic, rates, averages, extrema, frequency moments). A
    /// quantile root has no default: its request must name the metric. An
    /// exact request needs none.
    pub fn for_root(&self, root: &QueryRoot) -> Result<AccuracyTarget, AccuracyRequestError> {
        if let Some(metric) = self.metric {
            return Ok(self.target_in(metric));
        }
        if self.is_exact() {
            return Ok(self.target_in(ErrorMetric::ABSOLUTE));
        }
        root_metric(root).map(|metric| self.target_in(metric))
    }
}

/// The default metric of an intent's value, `None` for a quantile and for
/// intents without a listed default.
fn intent_metric(intent: &AggIntent) -> Option<ErrorMetric> {
    match intent {
        AggIntent::Count { .. } | AggIntent::TopK { .. } => Some(ErrorMetric::ABSOLUTE_STREAM_L1),
        AggIntent::Cardinality { .. } => Some(ErrorMetric::Relative),
        _ => None,
    }
}

fn root_metric(root: &QueryRoot) -> Result<ErrorMetric, AccuracyRequestError> {
    match root {
        QueryRoot::Operator(node) => node_metric(node),
        QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node))
        | QueryRoot::Scalar(ScalarExpr::ScalarSubquery(node)) => node_metric(node),
        QueryRoot::Scalar(_) => Ok(ErrorMetric::ABSOLUTE),
    }
}

/// The metric of `node`'s values, looking through operators that only
/// select, order or rename rows.
fn node_metric(node: &OperatorNode) -> Result<ErrorMetric, AccuracyRequestError> {
    let Operator::NonASAP(op) = &node.operator else {
        return Ok(ErrorMetric::ABSOLUTE);
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
        NonASAPOp::Aggregate { measures, .. } => {
            let mut metrics = Vec::new();
            for intent in measures {
                match intent {
                    AggIntent::Quantile { .. } => {
                        return Err(AccuracyRequestError::MissingQuantileMetric {
                            intent: "quantile",
                        })
                    }
                    AggIntent::HistogramQuantile { .. } => {
                        return Err(AccuracyRequestError::MissingQuantileMetric {
                            intent: "histogram_quantile",
                        })
                    }
                    intent => metrics.push(intent_metric(intent).unwrap_or(ErrorMetric::ABSOLUTE)),
                }
            }
            // Measures with different default metrics share one value-unit
            // statement.
            Ok(match metrics.split_first() {
                Some((first, rest)) if rest.iter().all(|metric| metric == first) => *first,
                _ => ErrorMetric::ABSOLUTE,
            })
        }
        _ => Ok(ErrorMetric::ABSOLUTE),
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

    /// The old workload forms still read, and name no metric; the spec
    /// form may name one.
    #[test]
    fn requested_accuracy_reads_old_and_spec_forms() {
        let read = |value: serde_json::Value| -> RequestedAccuracy {
            serde_json::from_value(value).unwrap()
        };
        assert_eq!(read(serde_json::json!("Exact")), RequestedAccuracy::exact());
        assert_eq!(
            read(serde_json::json!({"Epsilon": 0.01})),
            RequestedAccuracy::epsilon(0.01)
        );
        assert_eq!(
            read(serde_json::json!({"EpsilonDelta": {"epsilon": 0.01, "delta": 0.001}})),
            RequestedAccuracy::epsilon_delta(0.01, 0.001)
        );
        let rank = RequestedAccuracy::epsilon_delta(0.01, 0.001).with_metric(ErrorMetric::Rank);
        assert_eq!(
            read(
                serde_json::json!({"metric": "rank", "error": 0.01, "failure_probability": 0.001})
            ),
            rank
        );
        assert_eq!(read(serde_json::to_value(&rank).unwrap()), rank);
        assert!(
            serde_json::from_value::<RequestedAccuracy>(serde_json::json!({
                "EpsilonDelta": {"epsilon": 0.1, "delta": 0.01, "typo": true}
            }))
            .is_err()
        );
        assert!(serde_json::from_value::<RequestedAccuracy>(
            serde_json::json!({"error": 0.1, "typo": true})
        )
        .is_err());
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
    /// frequencies are L1-scaled absolute errors, distinct counts relative
    /// ones, anything else (a sum among them) absolute value errors.
    #[test]
    fn root_metric_defaults_follow_the_query_intent() {
        let request = RequestedAccuracy::epsilon(0.01);
        let metric = |intent: AggIntent| request.for_root(&aggregate(vec![intent])).unwrap().metric;
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
        assert_eq!(metric(AggIntent::Avg { col: None }), ErrorMetric::ABSOLUTE);
        assert_eq!(
            request
                .for_root(&QueryRoot::Scalar(ScalarExpr::literal_f64(1.0)))
                .unwrap()
                .metric,
            ErrorMetric::ABSOLUTE
        );
    }

    /// A quantile root's request must name its metric; a named metric or an
    /// exact request needs no default.
    #[test]
    fn a_quantile_root_must_name_its_metric() {
        let root = aggregate(vec![quantile()]);
        assert_eq!(
            RequestedAccuracy::epsilon(0.01).for_root(&root),
            Err(AccuracyRequestError::MissingQuantileMetric { intent: "quantile" })
        );
        assert_eq!(
            RequestedAccuracy::epsilon(0.01)
                .with_metric(ErrorMetric::Relative)
                .for_root(&root)
                .unwrap()
                .metric,
            ErrorMetric::Relative
        );
        assert!(RequestedAccuracy::exact()
            .for_root(&root)
            .unwrap()
            .is_exact());
    }

    /// Exact means zero error with certainty, whatever the metric; an
    /// ε of zero without a δ is not exact.
    #[test]
    fn exact_targets() {
        assert!(AccuracyTarget::exact().is_exact());
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
