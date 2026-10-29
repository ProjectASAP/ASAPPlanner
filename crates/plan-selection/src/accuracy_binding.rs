//! Binding the statistics a root guarantee leaves symbolic: from the data
//! workload and the operand-domain contracts.

use std::collections::HashMap;
use std::rc::Rc;

use asap_accuracy::composition::{
    parse_raw_data_samples_sign_statistic, parse_source_norm_statistic, INPUT_ROW_COUNT,
    PRODUCT_OPERAND_DOMAINS, STREAM_L1_NORM,
};
use asap_accuracy::estimators::{hll, univmon};
use asap_accuracy::OperandDomain;
use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    operand_domain_statistic, operand_guarantee, operand_of, project_column_guarantee,
    root_guarantee,
};
use asap_types::ir::operator::operator_properties::BinaryOpKind;
use asap_types::ir::properties::{AccuracyGuarantee, AccuracyMetric};
use asap_types::ir::scalar::ScalarValue;
use asap_types::ir::{BinaryOperator, NonASAPOp, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::workload::DataWorkload;

use crate::statistics::declared_non_negative;
use crate::PlanningModels;

/// The operator nodes a query root reads.
pub(crate) fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// [`root_guarantee`] with the statistics Stage 1 left symbolic bound:
/// - the input row count, `series` ([`series_read`] of `root`);
/// - each source's stream norm ([`source_norm_statistic`]), from the data
///   workload's per-source `stream_norms`;
/// - classic HLL's failure probability and the chance a UnivMon's top layer
///   is incomplete, from the data workload's per-source
///   `max_distinct_per_evaluation`;
/// - a product's or quotient's bound, from the deployment's enforced
///   operand domains ([`operand_domain_bounds`]).
///
/// Every data-workload fact is declared: the caller drops facts that no
/// longer hold before planning. Any other statistic stays unknown, and an
/// unknown statistic never meets a target.
pub(crate) fn bound_root_guarantee(
    root: &QueryRoot,
    series: Option<u64>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<AccuracyGuarantee> {
    let domains = operand_domain_bounds(root, series, data, models);
    let bind = |guarantee: &AccuracyGuarantee| {
        bind_statistics(guarantee, &|statistic| {
            workload_statistic(statistic, series, data)
                .or_else(|| domains.get(statistic).copied().flatten())
        })
    };
    match root_guarantee(root) {
        Some(guarantee) => Some(bind(&guarantee)),
        None => asap_logical_asap_optimization::pass1::per_computation_candidates::bind_guarantees(
            root, &bind,
        ),
    }
}

/// `guarantee` with the statistics `value` supplies bound. A sign gate
/// ([`raw_data_samples_sign_statistic`](asap_accuracy::composition::raw_data_samples_sign_statistic))
/// is an indicator that only 1 (proven) opens: any other value leaves it
/// unbound, so it can never read as a perfect bound.
pub(crate) fn bind_statistics(
    guarantee: &AccuracyGuarantee,
    value: &dyn Fn(&str) -> Option<f64>,
) -> AccuracyGuarantee {
    guarantee.bind(&|statistic| {
        let supplied = value(statistic);
        match parse_raw_data_samples_sign_statistic(statistic) {
            Some(_) => supplied.filter(|v| *v == 1.0),
            None => supplied,
        }
    })
}

/// A data-workload statistic Stage 1 left symbolic: the input row count
/// (`series`, the series of the sources the query reads), one source's
/// stream norm, or classic HLL's failure probability or a UnivMon's
/// incomplete-top-layer probability given one source's distinct bound,
/// from source facts that hold at any planning time, or the sign of one
/// metric's samples, from its declared type.
pub(crate) fn workload_statistic(
    statistic: &str,
    series: Option<u64>,
    data: &DataWorkload,
) -> Option<f64> {
    // The sign of one metric's samples: proven (1) when the metric is
    // declared a counter or declared non-negative; nothing is inferred from
    // its name.
    if let Some(metric) = parse_raw_data_samples_sign_statistic(statistic) {
        return declared_non_negative(data, metric).then_some(1.0);
    }
    if statistic == INPUT_ROW_COUNT {
        return series.map(|rows| rows as f64);
    }
    if let Some((precision, relative_error, source)) =
        hll::parse_classic_failure_statistic(statistic)
    {
        let bound = data.sources.get(source)?.max_distinct_per_evaluation?;
        return Some(hll::classic_failure_probability(
            precision,
            relative_error,
            bound,
        ));
    }
    if let Some((heap_size, layers, source)) =
        univmon::parse_incomplete_top_layer_statistic(statistic)
    {
        let bound = data.sources.get(source)?.max_distinct_per_evaluation?;
        return Some(univmon::incomplete_top_layer_probability(
            heap_size, layers, bound,
        ));
    }
    let (norm, source) = parse_source_norm_statistic(statistic)?;
    let norms = data.sources.get(source)?.stream_norms?;
    match norm {
        STREAM_L1_NORM => norms.l1,
        _ => norms.l2,
    }
}

/// One operand of a product or quotient, as Stage 3 finds its domain.
enum DomainOperand<'a> {
    /// A planned operator: the deployment's `OperandDomain` for it.
    Node(&'a Rc<OperatorNode>),
    /// A literal: itself.
    Literal(f64),
    /// Anything else: no domain.
    Other,
}

impl<'a> DomainOperand<'a> {
    fn of(expr: &'a ScalarExpr) -> Self {
        match expr {
            ScalarExpr::PromqlScalarFromVector(node) | ScalarExpr::ScalarSubquery(node) => {
                Self::Node(node)
            }
            ScalarExpr::Literal(ScalarValue::Float64(value)) => Self::Literal(*value),
            ScalarExpr::Literal(ScalarValue::Int64(value)) => Self::Literal(*value as f64),
            _ => Self::Other,
        }
    }

    fn domain(&self, models: &PlanningModels<'_>) -> Option<OperandDomain> {
        match self {
            Self::Node(node) => models.evidence.operand_domain(node),
            Self::Literal(value) => Some(OperandDomain {
                lower: *value,
                upper: *value,
            }),
            Self::Other => None,
        }
    }
}

/// A product or quotient whose bound Stage 1 left symbolic on its operands'
/// domains ([`operand_domain_statistic`]).
struct DomainOperation<'a> {
    statistic: &'static str,
    operands: [(DomainOperand<'a>, AccuracyGuarantee); 2],
}

/// Each operand-domain statistic in `root`, bound to the largest bound
/// among the operations that leave it symbolic, or `None` (unbound) when
/// any of them has no bound. Operations are found where the bound
/// originates (the same rule Stage 1 uses), in vector arithmetic, a scalar
/// root, and projection expressions. `series` is the root's input row
/// count ([`series_read`]).
fn operand_domain_bounds(
    root: &QueryRoot,
    series: Option<u64>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> HashMap<&'static str, Option<f64>> {
    let mut operations = Vec::new();
    if let QueryRoot::Scalar(expr) = root {
        scalar_domain_operations(expr, &|_| None, &mut operations);
    }
    let nodes: Vec<Rc<OperatorNode>> = root_operators(root)
        .iter()
        .flat_map(|node| OperatorNode::reachable(node))
        .collect();
    for node in &nodes {
        match node.non_asap() {
            Some(NonASAPOp::BinaryOp {
                operator:
                    BinaryOperator {
                        kind: BinaryOpKind::Arithmetic(op),
                        ..
                    },
                lhs,
                rhs,
                ..
            }) => {
                let (Some(l), Some(r)) = (operand_guarantee(lhs), operand_guarantee(rhs)) else {
                    continue;
                };
                if let Some(statistic) = operand_domain_statistic(op, &l, &r) {
                    operations.push(DomainOperation {
                        statistic,
                        operands: [(DomainOperand::Node(lhs), l), (DomainOperand::Node(rhs), r)],
                    });
                }
            }
            Some(NonASAPOp::Project { cols, child, .. }) => {
                for item in cols {
                    scalar_domain_operations(
                        &item.expr,
                        &|column| project_column_guarantee(child, column),
                        &mut operations,
                    );
                }
            }
            _ => {}
        }
    }
    let mut bounds: HashMap<&'static str, Option<f64>> = HashMap::new();
    for operation in &operations {
        let bound = domain_bound(operation, series, data, models);
        let entry = bounds.entry(operation.statistic).or_insert(Some(0.0));
        *entry = entry.zip(bound).map(|(a, b)| a.max(b));
    }
    bounds
}

/// The operations in `expr` (and its sub-expressions) whose bound Stage 1
/// left symbolic on operand domains.
fn scalar_domain_operations<'a>(
    expr: &'a ScalarExpr,
    column: &dyn Fn(usize) -> Option<AccuracyGuarantee>,
    operations: &mut Vec<DomainOperation<'a>>,
) {
    if let ScalarExpr::Arithmetic {
        op, left, right, ..
    } = expr
    {
        if let (Some(l), Some(r)) = (operand_of(left, column), operand_of(right, column)) {
            if let Some(statistic) = operand_domain_statistic(op, &l, &r) {
                operations.push(DomainOperation {
                    statistic,
                    operands: [(DomainOperand::of(left), l), (DomainOperand::of(right), r)],
                });
            }
        }
    }
    for child in expr.children() {
        scalar_domain_operations(child, column, operations);
    }
}

/// The bound of one product or quotient from its operands' domains:
/// - a product: `|x|·e_y + |y|·e_x + e_x·e_y`;
/// - a quotient: `(|x|·e_y + |y|·e_x) / (m·(m − e_y))` with `m` the least
///   `|y|`, which must exceed `e_y` (the denominator's domain is bounded
///   away from zero).
///
/// `|x|`, `|y|` are the domains' largest magnitudes, and `e` each
/// operand's absolute error (a relative or cardinality bound times that
/// magnitude) after binding its data-workload statistics (`series`, the
/// root's input row count). `None` when an operand has no domain or an
/// unbound error.
fn domain_bound(
    operation: &DomainOperation<'_>,
    series: Option<u64>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<f64> {
    let [(x, gx), (y, gy)] = &operation.operands;
    let (dx, dy) = (x.domain(models)?, y.domain(models)?);
    let bind =
        |g: &AccuracyGuarantee| g.bind(&|statistic| workload_statistic(statistic, series, data));
    let (gx, gy) = (bind(gx), bind(gy));
    let valid =
        |d: &OperandDomain| d.lower.is_finite() && d.upper.is_finite() && d.lower <= d.upper;
    if !(valid(&dx) && valid(&dy)) {
        return None;
    }
    let magnitude = |d: &OperandDomain| d.lower.abs().max(d.upper.abs());
    let (mx, my) = (magnitude(&dx), magnitude(&dy));
    let error = |g: &AccuracyGuarantee, m: f64| {
        let bound = g.spec.error.evaluate()?;
        match g.spec.accuracy_metric {
            _ if g.is_exact() => Some(0.0),
            AccuracyMetric::ABSOLUTE => Some(bound),
            AccuracyMetric::Relative => Some(bound * m),
            _ => None,
        }
    };
    let (ex, ey) = (error(&gx, mx)?, error(&gy, my)?);
    let bound = if operation.statistic == PRODUCT_OPERAND_DOMAINS {
        mx * ey + my * ex + ex * ey
    } else {
        let least = if dy.lower > 0.0 {
            dy.lower
        } else if dy.upper < 0.0 {
            -dy.upper
        } else {
            return None;
        };
        if least <= ey {
            return None;
        }
        (mx * ey + my * ex) / (least * (least - ey))
    };
    bound.is_finite().then_some(bound)
}
