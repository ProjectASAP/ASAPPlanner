//! #509 Stage 3, plan selection (MVP): the only stage that computes cost.
//!
//! Each CandidatePhysicalASAPDAG from Stage 2 is checked against every
//! query's accuracy target: the query's output meets it when its root
//! guarantee does (theoretical: Stage 1 composes it with the built-in
//! accuracy model, and this stage binds the statistics it left symbolic
//! from the data workload and the deployment's operand-domain contracts),
//! or when the deployment's empirical evidence shows this
//! candidate's output does (`AccuracyEvidenceProvider::output_guarantee`,
//! read only here). For a query with a target, Count-Min is admitted only
//! over weights proven non-negative; a query without one constrains nothing.
//! A miss rejects the candidate as invalid, with a reason; only this stage
//! discards valid candidates.
//! Every valid candidate is priced node by node over its physical DAG, and
//! the cheapest is selected. The rest are reported valid but costlier.
//! Selection prices every candidate it is given; a search that avoids
//! building every candidate (e.g. a dynamic program over target nesting) is
//! deferred to #671.
//!
//! ## Cost model: cost per second of wall time
//!
//! A candidate's cost is what running it costs per second ([`COST_UNIT`]),
//! so queries with different recurrences add up. Each node's price for one
//! computation is multiplied by how often it is computed per second
//! ([`NodeCost::per_second`]):
//!
//! * A `NotMaterialized` node is recomputed for each consumer that reads it
//!   and is not stored: it is charged once per consumer
//!   ([`NodeCost::multiplier`]) at the sum of their rates. Its consumers are
//!   the queries whose sub-DAG reaches it and the materialized nodes above
//!   it, counted without looking through a materialized node: a node below
//!   one is computed when that node is. Stage 2 does not duplicate a shared
//!   node, so a shared `NotMaterialized` input costs what reading it
//!   separately does (#509 Example 4).
//! * A `MatAtQuery` node is computed once for all the queries that reach
//!   it, at their combined rate: queries repeating at the same interval are
//!   evaluated together, so each distinct interval counts once.
//! * A `MatAtIngestion` node runs as data arrives: it is priced over one
//!   second of arriving rows, once per second, whatever its readers' rates.
//! * A query repeating every `I` ms runs `1000 / I` times per second, and an
//!   estimated rate runs at that rate. One-off, scheduled and unknown
//!   queries are spread over the calibration's `horizon_s` (default
//!   3600 s): `invocations / horizon_s` per second.
//!
//! Memory is charged only as state kept between computations: a
//! materialized node's state, `cost_per_retained_byte_second` per byte per
//! second, once for `MatAtQuery` and twice for `MatAtIngestion` (the window
//! being built and the completed one queries read). Memory a computation
//! holds only while it runs is not charged, and `cost_per_retained_byte` is
//! ignored: peak capacity belongs to cost model 2 (#683). A computation is
//! charged for its CPU operations and scanned bytes only.
//!
//! Prices come from [`crate::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count; summary build and estimation are priced as rows × sketch depth and
//! groups × k. Kept state is sized by its bytes: a sketch by its parameters, a
//! fixed-size exact accumulator (sum, count, min, max, rate, ...) as one
//! value per group, and exact state that grows with the data (quantile,
//! top-k, distinct count, L2, entropy keep every value or item) as one value
//! per input row of its group, assuming rows spread evenly over groups and,
//! for distinct items, that every row may be distinct. These numbers are
//! illustrative, not calibrated: a binary, join or concat node is priced from
//! its first input only (MVP; follow-up).
//! A cost error on any candidate aborts selection; it is reachable only from
//! bad declared rates. Latency bounds and deployment capabilities are not
//! checked yet.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::operator_properties::BinaryOpKind;
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::{
    ASAPOp, BinaryOperator, NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr,
};
use asap_types::post_asap::sketch::KLL_MAX_K;
use asap_types::post_asap::{
    AccuracyGuarantee, CompositionOperator, ErrorMetric, ExecutionTiming, FieldDataType,
    Materialization, SketchAlgorithm, SketchParams, SketchStatistic, WeightDomain,
};
use asap_types::pre_asap::{AggIntent, DataType, Reduction, ScalarValue, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{DataWorkload, MetricType, QueryRecurrence, RepeatedDemand};
use thiserror::Error;

use crate::accuracy::composition::{
    parse_samples_sign_statistic, parse_source_norm_statistic, DIVISION_OPERAND_DOMAINS,
    INPUT_ROW_COUNT, PRODUCT_OPERAND_DOMAINS, STREAM_L1_NORM,
};
use crate::accuracy::{OperandDomain, PropagationStats};
use crate::analytical_cost::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceCalibration, ResourceEstimate,
};
use crate::per_computation_candidates::{
    operand_domain_statistic, operand_guarantee, operand_of, project_column_guarantee,
    scalar_guarantee,
};
use crate::physical_operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::stage2_physical_asap_optimization::PhysicalCandidate;
use crate::PlanningModels;

/// Cost per second of wall time; one unit is one CPU-millisecond under
/// [`illustrative_calibration`].
pub const COST_UNIT: &str = "cost_per_second";
pub const COST_SOURCE: &str = "analytical-cost-v2 (illustrative statistics)";

/// The coefficients plan selection prices with until a deployment is
/// calibrated: 1 ns of CPU per operation, 0.1 ns per scanned byte,
/// 1 GB kept for a second costs 1/8 vCPU (125 CPU-ms), and one-off work is
/// spread over one hour.
pub fn illustrative_calibration() -> ResourceCalibration {
    ResourceCalibration {
        cost_per_cpu_op: 1e-6,
        cost_per_scan_byte: 1e-7,
        // Plan selection charges no memory per computation.
        cost_per_retained_byte: 0.0,
        cost_per_retained_byte_second: 1.25e-7,
        horizon_s: crate::analytical_cost::DEFAULT_HORIZON_S,
        version: "illustrative-v2".into(),
    }
}

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;

/// `cost` is the node's price per second: one computation's price times
/// `per_second`, the times it is computed per second, plus any kept state's
/// price ([`computations`], [`price`]). `multiplier` is the number of
/// consumers it is computed for (1 when materialized).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    pub cost: f64,
    pub multiplier: u64,
    pub per_second: f64,
    pub detail: String,
}

/// Whole-workload cost of one candidate: one entry per DAG node, `total` is
/// their sum.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateCost {
    pub total: f64,
    pub unit: &'static str,
    pub source: &'static str,
    pub per_node: BTreeMap<PhysicalASAPNodeId, NodeCost>,
}

/// A candidate that was not selected. `valid == false`: it failed a check;
/// `true`: it lost on cost, or tied and lost on enumeration order.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    pub id: String,
    pub valid: bool,
    pub reason: String,
}

/// Costs are present for valid candidates only.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    pub selected: String,
    pub costs: BTreeMap<String, CandidateCost>,
    pub rejected: Vec<Rejection>,
}

#[derive(Debug, Error)]
pub enum SelectionError {
    #[error(
        "candidate {candidate} has {roots} roots but {targets} accuracy targets and \
         {recurrences} recurrences were given"
    )]
    TargetCount {
        candidate: String,
        roots: usize,
        targets: usize,
        recurrences: usize,
    },
    #[error("candidate {candidate}, node {node:?}: {error}")]
    Cost {
        candidate: String,
        node: PhysicalASAPNodeId,
        error: AnalyticalCostError,
    },
    #[error("no valid candidate: {0:?}")]
    NoValidCandidate(Vec<Rejection>),
}

/// Stage 3 (plan selection): reject candidates that miss a query's accuracy
/// target, price the rest per second and select the cheapest (the first on
/// ties). `targets[i]` is the requirement of `candidate.roots[i]` (`None`
/// imposes none) and `recurrence[i]` how often it is evaluated.
pub fn select_plan(
    cands: &[PhysicalCandidate],
    targets: &[Option<AccuracyTarget>],
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Selection, SelectionError> {
    let mut costs = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut best: Option<(&str, f64)> = None;
    for candidate in cands {
        if candidate.roots.len() != targets.len() || candidate.roots.len() != recurrence.len() {
            return Err(SelectionError::TargetCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                targets: targets.len(),
                recurrences: recurrence.len(),
            });
        }
        if let Some(reason) = accuracy_violation(candidate, targets, data, &models) {
            rejected.push(Rejection {
                id: candidate.id.clone(),
                valid: false,
                reason,
            });
            continue;
        }
        let calibration = illustrative_calibration();
        let cost =
            price(&candidate.dag, recurrence, data, &calibration).map_err(|(node, error)| {
                SelectionError::Cost {
                    candidate: candidate.id.clone(),
                    node,
                    error,
                }
            })?;
        if best.is_none_or(|(_, total)| cheaper(cost.total, total)) {
            best = Some((&candidate.id, cost.total));
        }
        costs.insert(candidate.id.clone(), cost);
    }
    let Some((selected, best_total)) = best else {
        return Err(SelectionError::NoValidCandidate(rejected));
    };
    for candidate in cands {
        if candidate.id != selected && costs.contains_key(&candidate.id) {
            let total = costs[&candidate.id].total;
            let reason = if !cheaper(best_total, total) {
                format!(
                    "tied with the selected candidate {selected} at {total:.3} {COST_UNIT}; \
                     lost on enumeration order"
                )
            } else {
                format!("costlier: {total:.3} vs {best_total:.3} {COST_UNIT}")
            };
            rejected.push(Rejection {
                id: candidate.id.clone(),
                valid: true,
                reason,
            });
        }
    }
    Ok(Selection {
        selected: selected.to_string(),
        costs,
        rejected,
    })
}

/// Whether `cost` is cheaper than `best`. Totals that differ only by
/// floating-point rounding (summing the same charges in another grouping)
/// tie, and a tie keeps the earlier candidate.
fn cheaper(cost: f64, best: f64) -> bool {
    cost < best - best.abs() * 1e-9
}

/// The first query whose output is not shown to meet its target, as a
/// reason. An output meets its target when its theoretical root guarantee
/// (composed in Stage 1 with the built-in model) satisfies it, or when the
/// deployment's empirical evidence shows that this candidate's output does
/// ([`crate::accuracy::AccuracyEvidenceProvider::output_guarantee`]). A
/// certified estimate inside the plan is not enough on its own: a difference
/// or ratio of certified estimates can be unbounded. The root guarantee's
/// symbolic statistics are bound first ([`bound_root_guarantee`]); one still
/// unknown leaves the target unmet. Count-Min is admitted only over update
/// weights proven non-negative, which its bound assumes: an open
/// [`WeightDomain::SamplesOfMetric`] requirement is met when `data` declares
/// the metric a counter ([`confirmed_weights`]), and unmet otherwise.
fn accuracy_violation(
    candidate: &PhysicalCandidate,
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<String> {
    for (query, (root, target)) in candidate.roots.iter().zip(targets).enumerate() {
        let query = query + 1;
        let Some(target) = target else { continue };
        for node in root_operators(root)
            .iter()
            .flat_map(|node| OperatorNode::reachable(node))
        {
            let Operator::ASAP(ASAPOp::SummaryAgg { family, input, .. }) = &node.operator else {
                continue;
            };
            if matches!(family, FieldDataType::Sketch(kind, _)
                    if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap))
                && !matches!(
                    confirmed_weights(&input.weight_domain, data),
                    WeightDomain::NonNegative { .. }
                )
            {
                let unconfirmed = match &input.weight_domain {
                    WeightDomain::SamplesOfMetric { metric } => {
                        format!(" (samples of `{metric}`, which is not declared a counter)")
                    }
                    _ => String::new(),
                };
                return Some(format!(
                    "q{query}: {} needs non-negative update weights, and these are not proven \
                     non-negative{unconfirmed}",
                    family_name(family)
                ));
            }
        }
        let guarantee = bound_root_guarantee(root, data, models);
        let meets = |g: &AccuracyGuarantee| models.accuracy.satisfies(g, target);
        if guarantee.as_ref().is_some_and(meets)
            || models
                .evidence
                .output_guarantee(root)
                .as_ref()
                .is_some_and(meets)
        {
            continue;
        }
        return Some(match guarantee {
            Some(g) => format!(
                "q{query}: the output guarantees bound {:?}, failure probability {:?}, which \
                 misses target {target:?}, and the deployment has no evidence that it meets it",
                g.bound.evaluate(),
                g.failure_probability.evaluate(),
            ),
            None => format!(
                "q{query}: the output has no guarantee, and the deployment has no evidence \
                 that it meets target {target:?}"
            ),
        });
    }
    None
}

/// `domain` with an open [`WeightDomain::SamplesOfMetric`] requirement
/// answered from `data`'s declared metric types: proven when the metric is
/// declared a counter, unknown otherwise. No type is inferred from a name.
pub fn confirmed_weights(domain: &WeightDomain, data: &DataWorkload) -> WeightDomain {
    domain.confirmed(|metric| data.metric_types.get(metric) == Some(&MetricType::Counter))
}

/// `roots` with every open [`WeightDomain::SamplesOfMetric`] requirement
/// replaced by its answer from `data` ([`confirmed_weights`]): the form of a
/// selected plan the executor runs, which reads no data workload. The same
/// `Rc` where nothing changes.
pub fn with_confirmed_weights(roots: &[QueryRoot], data: &DataWorkload) -> Vec<QueryRoot> {
    fn confirm(
        node: &Rc<OperatorNode>,
        data: &DataWorkload,
        memo: &mut HashMap<*const OperatorNode, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(done) = memo.get(&Rc::as_ptr(node)) {
            return Rc::clone(done);
        }
        let mut changed = false;
        let mut operator = node.operator.map_children(|child| {
            let rebuilt = confirm(child, data, memo);
            changed |= !Rc::ptr_eq(&rebuilt, child);
            rebuilt
        });
        if let Operator::ASAP(ASAPOp::SummaryAgg { input, .. }) = &mut operator {
            let confirmed = confirmed_weights(&input.weight_domain, data);
            if confirmed != input.weight_domain {
                input.weight_domain = confirmed;
                changed = true;
            }
        }
        let rebuilt = if changed {
            Rc::new(
                OperatorNode::with_schema(operator, node.schema.clone())
                    .with_guarantee(node.guarantee.clone())
                    .with_timing(node.timing)
                    .with_materialization(node.materialization),
            )
        } else {
            Rc::clone(node)
        };
        memo.insert(Rc::as_ptr(node), Rc::clone(&rebuilt));
        rebuilt
    }
    let mut memo = HashMap::new();
    roots
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(n) => QueryRoot::Operator(confirm(n, data, &mut memo)),
            QueryRoot::Scalar(expr) => {
                QueryRoot::Scalar(expr.map_operator_refs(&mut |n| confirm(n, data, &mut memo)))
            }
        })
        .collect()
}

/// The operator nodes a query root reads.
fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

/// The theoretical guarantee of a query's output, as Stage 1 composed it:
/// the root node's own, or a scalar root's composed over the operators it
/// reads ([`scalar_guarantee`]).
fn root_guarantee(root: &QueryRoot) -> Option<AccuracyGuarantee> {
    match root {
        QueryRoot::Operator(node) => node.guarantee.clone(),
        QueryRoot::Scalar(expr) => scalar_guarantee(expr, &|_| None),
    }
}

/// [`root_guarantee`] with the statistics Stage 1 left symbolic bound:
/// - the input row count, from the data workload's input cardinality;
/// - each source's stream norm ([`source_norm_statistic`]), from the data
///   workload's per-source `stream_norms`;
/// - a product's or quotient's bound, from the deployment's enforced
///   operand domains ([`operand_domain_bounds`]).
///
/// Data-workload evidence is used only while fresh. Stage 3 has no planning
/// time and, by design, stays conservative: it uses only evidence that
/// holds at any planning time (no validity window); evidence with a
/// validity window is never used. Any other statistic stays unknown, and
/// an unknown statistic never meets a target.
fn bound_root_guarantee(
    root: &QueryRoot,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<AccuracyGuarantee> {
    let guarantee = root_guarantee(root)?;
    let domains = operand_domain_bounds(root, data, models);
    Some(guarantee.bind(&|statistic| {
        workload_statistic(statistic, data).or_else(|| domains.get(statistic).copied().flatten())
    }))
}

/// A data-workload statistic Stage 1 left symbolic: the input row count,
/// one source's stream norm, from evidence that holds at any planning time,
/// or the sign of one metric's samples, from its declared type.
fn workload_statistic(statistic: &str, data: &DataWorkload) -> Option<f64> {
    // Without a planning time, only evidence that cannot expire is fresh.
    const ANY_TIME: u64 = u64::MAX;
    // The sign of one metric's samples: proven (1) when the metric is
    // declared a counter; no type is inferred from its name.
    if let Some(metric) = parse_samples_sign_statistic(statistic) {
        return (data.metric_types.get(metric) == Some(&MetricType::Counter)).then_some(1.0);
    }
    if statistic == INPUT_ROW_COUNT {
        return data
            .input_cardinality
            .value_at(ANY_TIME)
            .map(|rows| *rows as f64);
    }
    let (norm, source) = parse_source_norm_statistic(statistic)?;
    let norms = data.stream_norms.value_at(ANY_TIME)?.get(source)?;
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
                max_samples: 1,
                contract: "literal".into(),
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
/// root, and projection expressions.
fn operand_domain_bounds(
    root: &QueryRoot,
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
        let bound = domain_bound(operation, data, models);
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
/// - a ratio of relative errors: the accuracy model's division rule;
/// - a product: `|x|·e_y + |y|·e_x + e_x·e_y`;
/// - a quotient: `(|x|·e_y + |y|·e_x) / (m·(m − e_y))` with `m` the least
///   `|y|`, which must exceed `e_y` (the denominator's domain is bounded
///   away from zero).
///
/// `|x|`, `|y|` are the domains' largest magnitudes, and `e` each
/// operand's absolute error (a relative or cardinality bound times that
/// magnitude) after binding its data-workload statistics. `None` when an
/// operand has no domain or an unbound error.
fn domain_bound(
    operation: &DomainOperation<'_>,
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Option<f64> {
    let [(x, gx), (y, gy)] = &operation.operands;
    let (dx, dy) = (x.domain(models)?, y.domain(models)?);
    let bind = |g: &AccuracyGuarantee| g.bind(&|statistic| workload_statistic(statistic, data));
    let (gx, gy) = (bind(gx), bind(gy));
    if operation.statistic == DIVISION_OPERAND_DOMAINS {
        let stats = PropagationStats {
            division_operand_domains: Some([dx, dy]),
            ..PropagationStats::default()
        };
        return models
            .accuracy
            .propagate(&CompositionOperator::ExactDivision, &[gx, gy], None, &stats)
            .ok()?
            .bound
            .evaluate();
    }
    let valid =
        |d: &OperandDomain| d.lower.is_finite() && d.upper.is_finite() && d.lower <= d.upper;
    if !(valid(&dx) && valid(&dy)) {
        return None;
    }
    let magnitude = |d: &OperandDomain| d.lower.abs().max(d.upper.abs());
    let (mx, my) = (magnitude(&dx), magnitude(&dy));
    let error = |g: &AccuracyGuarantee, m: f64| {
        let bound = g.bound.evaluate()?;
        match g.metric {
            _ if g.is_exact() => Some(0.0),
            ErrorMetric::AbsoluteValue => Some(bound),
            ErrorMetric::RelativeValue | ErrorMetric::Cardinality => Some(bound * m),
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

fn family_name(family: &FieldDataType) -> String {
    match family {
        FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
        FieldDataType::ExactAggregate(kind, _) => format!("exact {kind:?} accumulator"),
        other => format!("{other:?}"),
    }
}

/// Statistics the analytical model needs, derived once per workload.
struct Shape {
    series: u64,
    rows_per_ms: f64,
}

/// Evaluations per second of a node computed once for `queries` together.
/// Repeating queries with equal intervals are evaluated together, so each
/// interval counts once; estimated rates add. One-off, scheduled and unknown
/// queries run as one batch: the most invocations among them, spread over
/// `horizon_s`.
fn evaluation_rate<'r>(
    queries: impl IntoIterator<Item = &'r QueryRecurrence>,
    horizon_s: f64,
) -> Result<f64, AnalyticalCostError> {
    let mut intervals = BTreeSet::new();
    let mut estimated = 0.0;
    let mut invocations = 0u64;
    for recurrence in queries {
        match recurrence {
            QueryRecurrence::Repeated(
                RepeatedDemand::FixedInterval(interval)
                | RepeatedDemand::FixedIntervalAt { interval, .. },
            ) => {
                if interval.0 == 0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                intervals.insert(interval.0);
            }
            QueryRecurrence::Repeated(RepeatedDemand::EstimatedRate(estimate)) => {
                let rate = estimate.expected_rate.0;
                if !rate.is_finite() || rate < 0.0 {
                    return Err(AnalyticalCostError::InvalidRecurrence);
                }
                estimated += rate;
            }
            QueryRecurrence::Repeated(RepeatedDemand::Scheduled(times)) => {
                invocations = invocations.max(times.len() as u64);
            }
            QueryRecurrence::OneTime {
                invocations: count, ..
            } => invocations = invocations.max(*count),
            QueryRecurrence::Unknown => invocations = invocations.max(1),
        }
    }
    let repeated: f64 = intervals.into_iter().map(|ms| 1_000.0 / ms as f64).sum();
    Ok(repeated + estimated + invocations as f64 / horizon_s)
}

/// The nodes each query reads first, in query order: query `i` is the scalar
/// root keyed `i` when there is one, else the next operator root.
fn query_starts(dag: &PhysicalASAPDAG) -> Vec<Vec<PhysicalASAPNodeId>> {
    let mut roots = dag.roots.iter();
    (0..dag.roots.len() + dag.scalar_roots.len())
        .map(|query| {
            match dag
                .scalar_roots
                .iter()
                .find(|(position, _)| *position == query)
            {
                Some((_, expr)) => expr.operator_refs().into_iter().copied().collect(),
                None => roots.next().into_iter().copied().collect(),
            }
        })
        .collect()
}

/// How often each node of `dag` is computed: `(multiplier, per_second)`
/// (module doc, "Cost model"). A materialized node is computed once for all
/// its readers: once per second at ingestion time (`MatAtIngestion`), or at
/// the combined rate of the queries reaching it (`MatAtQuery`,
/// [`evaluation_rate`]). A `NotMaterialized` node is computed once per
/// consumer that reaches it, at that consumer's rate. A consumer is a query
/// (operator or scalar root), at its own rate, or a materialized node, at
/// that node's rate; a walk stops at the first materialized node it meets,
/// and a node read several times within one consumer's walk counts once for
/// it. Its `multiplier` is that number of consumers.
fn computations(
    dag: &PhysicalASAPDAG,
    recurrence: &[QueryRecurrence],
    horizon_s: f64,
) -> Result<HashMap<PhysicalASAPNodeId, (u64, f64)>, AnalyticalCostError> {
    let materialized: HashMap<_, _> = dag
        .nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
        .map(|n| (n.id, n.materialization))
        .collect();
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    let starts = query_starts(dag);
    // The queries reaching each node, through any node.
    let mut reached: HashMap<PhysicalASAPNodeId, Vec<usize>> = HashMap::new();
    for (query, start) in starts.iter().enumerate() {
        let mut stack = start.clone();
        while let Some(id) = stack.pop() {
            let queries = reached.entry(id).or_default();
            if queries.last() == Some(&query) {
                continue;
            }
            queries.push(query);
            stack.extend(inputs(id));
        }
    }
    let mut rate_of_materialized = HashMap::new();
    for (&id, materialization) in &materialized {
        let rate = match materialization {
            Materialization::MatAtIngestion(_) => 1.0,
            _ => evaluation_rate(
                reached
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .filter_map(|&q| recurrence.get(q)),
                horizon_s,
            )?,
        };
        rate_of_materialized.insert(id, rate);
    }
    // Each consumer's starting nodes and its rate.
    let mut consumers = Vec::new();
    for (query, start) in starts.into_iter().enumerate() {
        let rate = evaluation_rate(recurrence.get(query), horizon_s)?;
        consumers.push((start, rate));
    }
    for (&id, &rate) in &rate_of_materialized {
        consumers.push((inputs(id).collect(), rate));
    }
    let mut count: HashMap<_, (u64, f64)> = HashMap::new();
    for (start, rate) in consumers {
        let mut seen = HashSet::new();
        let mut stack: Vec<PhysicalASAPNodeId> = start;
        while let Some(id) = stack.pop() {
            if materialized.contains_key(&id) || !seen.insert(id) {
                continue;
            }
            let entry = count.entry(id).or_default();
            entry.0 += 1;
            entry.1 += rate;
            stack.extend(inputs(id));
        }
    }
    Ok(dag
        .nodes
        .iter()
        .map(|n| {
            let times = match rate_of_materialized.get(&n.id) {
                Some(&rate) => (1, rate),
                None => count.get(&n.id).copied().unwrap_or((0, 0.0)),
            };
            (n.id, times)
        })
        .collect())
}

/// Price every node of `dag` per second of wall time (module doc, "Cost
/// model"). Nodes are exported children first, so each node's input
/// statistics are known when it is reached. A query-time node is priced per
/// computation over the rows its time ranges keep, times how often it is
/// computed per second ([`computations`]); an ingestion-time node over one
/// second of arriving rows. A materialized node's kept state is charged per
/// byte per second.
fn price(
    dag: &PhysicalASAPDAG,
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    calibration: &ResourceCalibration,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let first = dag.nodes.first().map(|n| n.id).unwrap_or_default();
    calibration.validate().map_err(|error| (first, error))?;
    let per_computation = ResourceCalibration {
        cost_per_retained_byte: 0.0,
        ..calibration.clone()
    };
    let series = data
        .input_cardinality
        .value
        .unwrap_or(DEFAULT_SERIES)
        .max(1);
    let shape = Shape {
        series,
        rows_per_ms: data
            .ingestion_rate
            .value
            .map_or(DEFAULT_ROWS_PER_SECOND, |rate| rate.0)
            / 1_000.0,
    };
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let multipliers =
        computations(dag, recurrence, calibration.horizon_s).map_err(|error| (first, error))?;
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
    for node in &dag.nodes {
        let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
        let inputs: Vec<_> = dag
            .edges
            .iter()
            .filter(|e| e.consumer == node.id)
            .map(|e| output[&e.producer])
            .collect();
        let input = inputs
            .first()
            .copied()
            .unwrap_or(EdgeStatistics { rows: 1, bytes: 1 });
        let width = row_bytes(&node.output_schema);
        let edge = |rows: u64| EdgeStatistics {
            rows,
            bytes: rows * width,
        };
        let unary = |output| UnaryEdgeStatistics {
            input,
            output,
            promql: None,
        };
        let groups = |reduction: &Reduction| {
            let groups = match reduction {
                Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => 1,
                Reduction::Reduce(keys) if !keys.is_without() => DEFAULT_GROUP_COUNT,
                _ => shape.series,
            };
            // One second of arriving rows does not bound the groups a
            // maintained state holds.
            match ingestion {
                true => groups,
                false => groups.min(input.rows.max(1)),
            }
        };
        let (out, estimate, detail) = match &node.payload {
            Payload::NonASAP(operator) => match operator {
                NonASAPOp::Scan { .. } => {
                    // At ingestion time, one second of arriving rows; at
                    // query time, what its time range keeps.
                    let lookback = dag
                        .edges
                        .iter()
                        .filter(|e| e.producer == node.id)
                        .filter_map(|e| match &nodes[&e.consumer].payload {
                            Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
                                Some(range.as_millis() as u64)
                            }
                            _ => None,
                        })
                        .max()
                        .unwrap_or(DEFAULT_LOOKBACK_MS);
                    let lookback = if ingestion { 1_000 } else { lookback };
                    let out = edge(((shape.rows_per_ms * lookback as f64).round() as u64).max(1));
                    let estimate = estimate_operator(
                        PhysicalOperator::Scan,
                        OperatorStatistics::Scan {
                            edges: UnaryEdgeStatistics {
                                input: out,
                                output: out,
                                promql: None,
                            },
                            source_read_bytes: out.bytes,
                        },
                    );
                    (out, estimate, format!("scan {} samples", out.rows))
                }
                NonASAPOp::Aggregate {
                    reduction,
                    measures,
                    ..
                } => {
                    let group_count = groups(reduction);
                    let keys = match reduction {
                        Reduction::Reduce(keys) => keys.keys().len() as u64,
                        _ => 1,
                    };
                    let out = edge(group_count);
                    let held = input.rows.div_ceil(group_count.max(1));
                    let estimate = estimate_operator(
                        PhysicalOperator::HashAggregate {
                            grouping_key_count: keys,
                            accumulator_count: measures.len().max(1) as u64,
                        },
                        OperatorStatistics::HashAggregate {
                            edges: unary(out),
                            group_count,
                            key_bytes: 16 * keys,
                            accumulator_bytes_per_group: measures
                                .iter()
                                .map(|m| if keeps_every_value(m) { 8 * held } else { 8 })
                                .sum::<u64>()
                                .max(8),
                        },
                    );
                    (
                        out,
                        estimate,
                        format!(
                            "hash aggregate {} rows into {group_count} groups",
                            input.rows
                        ),
                    )
                }
                NonASAPOp::Sort {
                    keys, partition_by, ..
                } => {
                    let partitions = if partition_by.keys().is_empty() {
                        1
                    } else {
                        DEFAULT_GROUP_COUNT.min(input.rows.max(1))
                    };
                    let estimate = estimate_operator(
                        PhysicalOperator::InMemoryComparisonSort {
                            ordering_key_count: keys.len() as u64,
                            partitioned: partitions > 1,
                        },
                        OperatorStatistics::InMemoryComparisonSort {
                            edges: unary(input),
                            input_partitioning: split(input, partitions),
                        },
                    );
                    (
                        input,
                        estimate,
                        format!("sort {} rows in {partitions} partitions", input.rows),
                    )
                }
                NonASAPOp::Limit {
                    n,
                    offset,
                    partition_by,
                    ..
                } => {
                    let partitions = if partition_by.keys().is_empty() {
                        1
                    } else {
                        DEFAULT_GROUP_COUNT
                    };
                    let limit = n.map_or(u64::MAX, |n| (n as u64).saturating_mul(partitions));
                    let offset = (*offset as u64).saturating_mul(partitions);
                    let out = edge(input.rows.saturating_sub(offset).min(limit));
                    let estimate = estimate_operator(
                        PhysicalOperator::Limit { limit, offset },
                        OperatorStatistics::Limit { edges: unary(out) },
                    );
                    (out, estimate, format!("limit to {} rows", out.rows))
                }
                other => {
                    let estimate = estimate_operator(
                        PhysicalOperator::PassThrough,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    );
                    let name = match other {
                        NonASAPOp::TimeRange { range, .. } => format!("time range {range:?}"),
                        _ => "operator".into(),
                    };
                    (input, estimate, format!("{name}: pass {} rows", input.rows))
                }
            },
            Payload::ASAP(ASAPOp::SummaryAgg {
                family, reduction, ..
            }) => {
                let group_count = groups(reduction);
                let (depth, state_bytes) = summary_shape(family);
                let out = EdgeStatistics {
                    rows: group_count,
                    bytes: group_count * state_bytes,
                };
                let ops = input.rows as f64 * depth;
                (
                    out,
                    Ok(ResourceEstimate::new(ops, out.bytes, 0)),
                    format!(
                        "build {} into {group_count} states: {} rows x depth {depth}",
                        family_name(family),
                        input.rows
                    ),
                )
            }
            Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
                let per_group = match query {
                    SketchStatistic::TopK { k } => *k as u64,
                    _ => 1,
                };
                let out = edge(input.rows * per_group);
                (
                    out,
                    Ok(ResourceEstimate::new(out.rows as f64, 0, 0)),
                    format!("estimate {} groups x {per_group}", input.rows),
                )
            }
            Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
                edge(input.rows),
                Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
                format!("finalize {} accumulators", input.rows),
            ),
            _ => (
                edge(input.rows),
                Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
                format!("{} rows", input.rows),
            ),
        };
        let estimate = estimate.map_err(|error| (node.id, error))?;
        // A computation is charged for its CPU work and scanned bytes; memory
        // only as kept state, below.
        let cost = estimate
            .calibrated_cost(&per_computation)
            .map_err(|error| (node.id, error))?;
        let (multiplier, per_second) = multipliers[&node.id];
        let mut cost = cost * per_second;
        let mut detail = format!("{detail}; x {per_second:.4}/s");
        // State kept between computations: the window being built and the
        // completed one at ingestion time, the kept output at query time.
        // The state is what the node holds: its output, or the values an
        // exact aggregate keeps per group when that is more.
        let state = out.bytes.max(estimate.peak_memory_bytes());
        let kept = match node.materialization {
            Materialization::NotMaterialized => 0,
            Materialization::MatAtIngestion(_) => 2 * state,
            _ => state,
        };
        if kept > 0 {
            cost += calibration.cost_per_retained_byte_second * kept as f64;
            detail += &format!("; keeps {kept} bytes");
        }
        if !cost.is_finite() {
            return Err((node.id, AnalyticalCostError::Overflow));
        }
        output.insert(node.id, out);
        per_node.insert(
            node.id,
            NodeCost {
                cost,
                multiplier,
                per_second,
                detail,
            },
        );
    }
    Ok(CandidateCost {
        total: per_node.values().map(|n| n.cost).sum(),
        unit: COST_UNIT,
        source: COST_SOURCE,
        per_node,
    })
}

/// `input` split as evenly as integers allow into `partitions` parts.
fn split(input: EdgeStatistics, partitions: u64) -> PartitionStatistics {
    let part = |total: u64, i: u64| total / partitions + u64::from(i < total % partitions);
    PartitionStatistics {
        partitions: (0..partitions)
            .map(|i| EdgeStatistics {
                rows: part(input.rows, i),
                bytes: part(input.bytes, i),
            })
            .collect(),
    }
}

/// Plain values are 8 bytes, strings 16; summary columns are sized apart.
fn row_bytes(schema: &Schema) -> u64 {
    schema
        .fields
        .iter()
        .map(|f| match &f.dtype {
            FieldDataType::Plain(DataType::Utf8) => 16,
            _ => 8,
        })
        .sum::<u64>()
        .max(1)
}

/// Exact state that keeps every value or item of its group, so grows with
/// the data; other exact accumulators are fixed-size.
fn keeps_every_value(measure: &AggIntent) -> bool {
    matches!(
        measure,
        AggIntent::Quantile { .. }
            | AggIntent::TopK { .. }
            | AggIntent::Cardinality { .. }
            | AggIntent::FrequencyL2 { .. }
            | AggIntent::FrequencyEntropy { .. }
    )
}

/// Update operations per input row and bytes per state, from the sketch's
/// parameters as `asap_sketchlib` allocates it:
/// - Count-Min / Count-Sketch: `depth × width` 8-byte counters, plus a
///   24-byte entry per heap slot for the top-k variants;
/// - KLL: the 8-byte item array it allocates up front ([`kll_items`]);
/// - DDSketch: one 8-byte count per bin it touches over an assumed nine
///   decades of values ([`ddsketch_bins`]);
/// - HLL: one 1-byte register per bucket, `2^precision`;
/// - KMV: its `k` smallest 64-bit hashes (Theta, which candidate generation
///   does not offer because `asap_sketchlib` has no Theta sketch, likewise);
/// - UnivMon: per layer, a `rows × cols` Count-Sketch of 8-byte counters and
///   a heap of `heap_size` 24-byte entries.
///
/// Update operations per input row: one counter per row of depth for
/// Count-Min and Count-Sketch, plus one heap update for their top-k variants;
/// for UnivMon, the same per layer it updates ([`univmon_expected_layers`]);
/// 1 for every other sketch.
fn summary_shape(family: &FieldDataType) -> (f64, u64) {
    match family {
        FieldDataType::Sketch(kind, _) => match kind.params() {
            SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
                (f64::from(*depth), 8 * u64::from(*width) * u64::from(*depth))
            }
            SketchParams::CmsWithHeap {
                width,
                depth,
                heap_size,
            }
            | SketchParams::CountSketchWithHeap {
                width,
                depth,
                heap_size,
            } => (
                f64::from(*depth) + 1.0,
                8 * u64::from(*width) * u64::from(*depth) + 24 * u64::from(*heap_size),
            ),
            SketchParams::Kll { k, .. } => (1.0, 8 * kll_items(*k)),
            SketchParams::DDSketch { alpha } => (1.0, ddsketch_bins(*alpha).saturating_mul(8)),
            SketchParams::Hll { precision } => (1.0, 1u64 << (*precision).min(63)),
            SketchParams::Kmv { k } | SketchParams::Theta { k } => (1.0, 8 * u64::from(*k)),
            SketchParams::UnivMon {
                heap_size,
                sketch_rows,
                sketch_cols,
                layers,
            } => (
                univmon_expected_layers(*layers) * (f64::from(*sketch_rows) + 1.0),
                u64::from(*layers)
                    * (8 * u64::from(*sketch_rows) * u64::from(*sketch_cols)
                        + 24 * u64::from(*heap_size)),
            ),
        },
        _ => (1.0, 8),
    }
}

/// Items `asap_sketchlib`'s KLL allocates up front for `k`
/// (`KLL::init_internal` → `compute_max_capacity(k, 8)`): over its 61 levels,
/// `max(ceil(k · (2/3)^i), 8)` each, with `k` capped at [`KLL_MAX_K`] as the
/// library caps it. 1,206 items at `k` = 269.
fn kll_items(k: u32) -> u64 {
    const LEVELS: usize = 61;
    const MIN_LEVEL_CAPACITY: f64 = 8.0;
    let k = f64::from(k.min(KLL_MAX_K));
    let mut scale = 1.0_f64;
    let mut total = 0;
    for _ in 0..LEVELS {
        total += (k * scale).ceil().max(MIN_LEVEL_CAPACITY) as u64;
        scale *= 2.0 / 3.0;
    }
    total
}

/// Layers a UnivMon row updates in expectation. `asap_sketchlib` updates
/// layers `0..=b`, with bottom layer `b` the index of the first zero among
/// hash bits `1..layers`, capped at `layers - 1`; so it reaches layer `j`
/// with probability `2^-j`, and the expectation is
/// `sum_{j < layers} 2^-j = 2 - 2^(1 - layers)`.
fn univmon_expected_layers(layers: u8) -> f64 {
    2.0 - 2f64.powi(1 - i32::from(layers.max(1)))
}

/// Ratio of the largest to the smallest positive value a DDSketch is assumed
/// to see: nine decades, e.g. latencies from 1 ns to 1 s. An assumption, not
/// workload evidence: the data workload declares no value range.
const DDSKETCH_ASSUMED_VALUE_RANGE: f64 = 1e9;

/// Bins a DDSketch with relative accuracy `alpha` touches, assuming its
/// values span nine decades ([`DDSKETCH_ASSUMED_VALUE_RANGE`] = 1e9): its
/// bins grow geometrically by `gamma = (1 + alpha) / (1 - alpha)`, so
/// `ceil(ln(1e9) / ln(gamma))`, e.g. 1,037 bins at α = 0.01. The count is
/// proportional to the decades spanned, so values over three decades would
/// need a third of these bins and over twelve decades four thirds.
/// `asap_sketchlib`'s store has no bin limit; the cap is the bins spanning
/// every value it can index.
fn ddsketch_bins(alpha: f64) -> u64 {
    let log_gamma = ((1.0 + alpha) / (1.0 - alpha)).ln();
    let (min, max) = asap_sketchlib::sketches::ddsketch::ddsketch_indexable_bounds(alpha);
    let indexable = (max.ln() - min.ln()) / log_gamma;
    (DDSKETCH_ASSUMED_VALUE_RANGE.ln() / log_gamma)
        .min(indexable)
        .ceil()
        .max(1.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accuracy::AccuracyEvidenceProvider;
    use crate::stage2_physical_asap_optimization::optimize_physical_asap;
    use crate::test_support::lower_promql;
    use asap_types::ir::{ExprSemantics, ScalarExpr};
    use asap_types::post_asap::{
        BoundExpr, ErrorMetric, GroupingStrategy, ProbabilityExpr, SketchKind,
    };
    use asap_types::pre_asap::expr_ir::ArithmeticOpKind;
    use asap_types::workload::{Evidence, Rate};

    /// `n` queries repeating every 10 s, as Example 1's panels do.
    fn every_10s(n: usize) -> Vec<QueryRecurrence> {
        vec![repeating(10_000); n]
    }

    /// A query repeating every `interval_ms`.
    fn repeating(interval_ms: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
            asap_types::workload::RepetitionInterval(interval_ms),
        ))
    }

    fn data() -> DataWorkload {
        DataWorkload {
            ingestion_rate: Evidence {
                value: Some(Rate(10_000.0)),
                ..Default::default()
            },
            input_cardinality: Evidence {
                value: Some(10_000),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Exact (P1), whole-expression CMS+heap (P2) and CountSketch+heap (P3) realizations of
    /// one approximate top-k query, in Pass 1 catalog order.
    fn candidates() -> Vec<PhysicalCandidate> {
        let root = lower_promql(
            "topk by (job) (10, sum_over_time(m[1m]))",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.001,
            },
        );
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let topk = inventory
            .targets
            .iter()
            .position(|t| t.choices.candidates.len() > 2)
            .unwrap();
        [0, 1, 2]
            .into_iter()
            .map(|index| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[topk] = index;
                let roots: Vec<_> = crate::per_computation_candidates::compose_logical_candidate(
                    &inventory, &choice,
                )
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
                let mut candidate = optimize_physical_asap("L", &roots).unwrap();
                candidate.id = format!("P{}", index + 1);
                candidate
            })
            .collect()
    }

    /// A summary whose analytical guarantee misses the target is rejected as
    /// invalid with a reason; the exact plan is then selected. (Top-K
    /// estimates carry no guarantee since #575, so a quantile summary is used.)
    #[test]
    fn accuracy_failing_candidate_is_rejected_as_invalid() {
        let planned = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let candidates = all_candidates("quantile_over_time(0.99, m[1m])", &planned);
        let strict = AccuracyTarget::EpsilonDelta {
            epsilon: 1e-6,
            delta: 1e-9,
        };
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());
        let selection = select_plan(
            &candidates,
            &[Some(strict)],
            &every_10s(1),
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(!summaries.contains(&selection.selected.as_str()));
        for id in &summaries {
            let rejection = selection.rejected.iter().find(|r| r.id == *id).unwrap();
            assert!(!rejection.valid);
            assert!(
                rejection.reason.contains("misses target"),
                "{}",
                rejection.reason
            );
            assert!(!selection.costs.contains_key(*id));
        }
    }

    /// Totals that differ only by rounding tie, so the earlier candidate is
    /// kept; a real difference is cheaper.
    #[test]
    fn totals_differing_by_rounding_tie() {
        // One unit in the last place apart, as the same charges summed in
        // another grouping can be.
        let separate = 0.336006_f64;
        let together = f64::from_bits(separate.to_bits() - 1);
        assert!(!cheaper(separate, together));
        assert!(!cheaper(together, separate));
        assert!(cheaper(0.3, 0.4));
        assert!(!cheaper(0.4, 0.3));
    }

    /// For a query with a target, Count-Min over weights not proven
    /// non-negative is invalid.
    #[test]
    fn count_min_over_signed_weights_is_rejected_as_invalid() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = select_plan(
            &candidates(),
            &[Some(target)],
            &every_10s(1),
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert!(!p2.valid);
        assert!(p2.reason.contains("non-negative"), "{}", p2.reason);
    }

    /// Count-Min over raw samples of `m` is admitted only when the data
    /// workload declares `m` a counter: a gauge, an undeclared metric or a
    /// counter under another name leaves the requirement unmet. Admitted,
    /// the top-k still needs evidence about its output.
    #[test]
    fn count_min_over_samples_needs_a_declared_counter() {
        use asap_types::workload::MetricType;
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let with_types = |types: &[(&str, MetricType)]| DataWorkload {
            metric_types: types.iter().map(|(m, t)| (m.to_string(), *t)).collect(),
            ..data()
        };
        for (types, admitted) in [
            (vec![("m", MetricType::Counter)], true),
            (vec![("m", MetricType::Gauge)], false),
            (vec![("m_total", MetricType::Counter)], false),
            (vec![], false),
        ] {
            let data = with_types(&types);
            let selection = select_plan(
                &candidates(),
                &[Some(target.clone())],
                &every_10s(1),
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
            assert!(!p2.valid);
            assert_eq!(
                !p2.reason.contains("non-negative"),
                admitted,
                "{types:?}: {}",
                p2.reason
            );
            if admitted {
                assert!(p2.reason.contains("no guarantee"), "{}", p2.reason);
            } else {
                assert!(
                    p2.reason.contains("not declared a counter"),
                    "{}",
                    p2.reason
                );
            }
        }
    }

    /// A relative bound gated on the sign of one metric's samples is bound
    /// when the data workload declares that metric a counter, and stays
    /// unknown for a gauge, an undeclared metric or a counter by another
    /// name.
    #[test]
    fn sign_of_counter_samples_is_bound_from_the_declared_type() {
        use crate::accuracy::composition::samples_sign_statistic;
        let gated = AccuracyGuarantee {
            metric: ErrorMetric::RelativeValue,
            bound: BoundExpr::Product {
                factors: vec![
                    BoundExpr::Constant { value: 0.01 },
                    BoundExpr::Unknown {
                        statistic: samples_sign_statistic("m"),
                    },
                ],
            },
            failure_probability: ProbabilityExpr::Constant { value: 0.001 },
            provenance: vec![],
        };
        let node = lower_promql("sum(m)", AccuracyTarget::Exact);
        let root = QueryRoot::Operator(Rc::new((*node).clone().with_guarantee(Some(gated))));
        let with = |types: &[(&str, MetricType)]| DataWorkload {
            metric_types: types.iter().map(|(m, t)| (m.to_string(), *t)).collect(),
            ..data()
        };
        let bound = |data: &DataWorkload| {
            bound_root_guarantee(&root, data, &PlanningModels::builtin())
                .unwrap()
                .bound
                .evaluate()
        };
        assert_eq!(bound(&with(&[("m", MetricType::Counter)])), Some(0.01));
        for types in [
            vec![("m", MetricType::Gauge)],
            vec![("m_total", MetricType::Counter)],
            vec![],
        ] {
            assert_eq!(bound(&with(&types)), None, "{types:?}");
        }
    }

    /// The selected plan's open requirement becomes a proof when confirmed
    /// and stays unproven otherwise; the executor reads no data workload.
    #[test]
    fn confirmed_weights_rewrite_only_the_summary_update() {
        use asap_types::post_asap::NonNegativeWeightProof;
        use asap_types::workload::MetricType;
        let p2 = &candidates()[1];
        let domains = |roots: &[QueryRoot]| -> Vec<WeightDomain> {
            roots
                .iter()
                .flat_map(root_operators)
                .flat_map(OperatorNode::reachable)
                .filter_map(|n| match n.asap() {
                    Some(ASAPOp::SummaryAgg { input, .. }) => Some(input.weight_domain.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            domains(&p2.roots),
            [WeightDomain::SamplesOfMetric { metric: "m".into() }]
        );
        let counter = DataWorkload {
            metric_types: [("m".to_string(), MetricType::Counter)].into(),
            ..data()
        };
        let confirmed = with_confirmed_weights(&p2.roots, &counter);
        assert_eq!(
            domains(&confirmed),
            [WeightDomain::NonNegative {
                proof: NonNegativeWeightProof::CounterSamples
            }]
        );
        assert_eq!(
            domains(&with_confirmed_weights(&p2.roots, &data())),
            [WeightDomain::UnknownOrSigned]
        );
        // Nothing open: the same roots.
        let exact = &candidates()[0];
        let QueryRoot::Operator(before) = &exact.roots[0] else {
            panic!()
        };
        let QueryRoot::Operator(after) = &with_confirmed_weights(&exact.roots, &counter)[0] else {
            panic!()
        };
        assert!(Rc::ptr_eq(before, after));
    }

    /// A query without a target constrains nothing: the Count-Min
    /// candidate is admitted and priced.
    #[test]
    fn count_min_is_admitted_without_a_target() {
        let selection = select_plan(
            &candidates(),
            &[None],
            &every_10s(1),
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert!(selection.costs.contains_key("P2"));
        assert!(selection.rejected.iter().all(|r| r.valid));
    }

    /// Each node is priced under its DAG id, and the total is the sum;
    /// every candidate is either selected or rejected as costlier. The
    /// CountSketch+heap estimate has no analytical guarantee, so deployment
    /// evidence admits it.
    #[test]
    fn per_node_costs_cover_the_dag_and_sum_to_the_total() {
        let candidates = candidates();
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = select_plan(
            &candidates,
            &[Some(target)],
            &every_10s(1),
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for candidate in candidates.iter().filter(|c| c.id != "P2") {
            let cost = &selection.costs[&candidate.id];
            let ids: Vec<_> = candidate.dag.nodes.iter().map(|n| n.id).collect();
            let mut keys: Vec<_> = cost.per_node.keys().copied().collect();
            keys.sort();
            let mut sorted = ids.clone();
            sorted.sort();
            assert_eq!(keys, sorted);
            let sum: f64 = cost.per_node.values().map(|n| n.cost).sum();
            assert_eq!(cost.total, sum);
            assert!(cost.total > 0.0);
        }
        let costlier: Vec<_> = selection.rejected.iter().filter(|r| r.valid).collect();
        assert_eq!(costlier.len(), 1);
        assert_ne!(costlier[0].id, selection.selected);
    }

    /// Query 1 is `sum(...)` over query 2's output. Every node reached by
    /// both queries is `NotMaterialized`, so recomputed for each: charged
    /// twice. Materializing query 2's root (`MatAtQuery`) charges it once,
    /// and the nodes below it once, since they run only when it does.
    #[test]
    fn not_materialized_shared_nodes_are_charged_per_query() {
        let outer = lower_promql("sum(sum_over_time(m[1m]))", AccuracyTarget::Exact);
        let inner = outer.children()[0].clone();
        let mut candidate = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(outer), QueryRoot::Operator(inner)],
        )
        .unwrap();
        let [outer, inner] = candidate.dag.roots[..] else {
            panic!("two roots")
        };
        // Kept state is priced apart; this compares computations only.
        let calibration = ResourceCalibration {
            cost_per_retained_byte_second: 0.0,
            ..illustrative_calibration()
        };
        let cost = price(&candidate.dag, &every_10s(2), &data(), &calibration).unwrap();
        let multipliers: BTreeMap<_, _> = cost
            .per_node
            .iter()
            .map(|(id, c)| (*id, c.multiplier))
            .collect();
        assert_eq!(multipliers[&outer], 1);
        assert_eq!(multipliers[&inner], 2);
        assert!(multipliers.len() > 2, "nodes below the shared root");
        for (id, times) in &multipliers {
            if *id != outer {
                assert_eq!(*times, 2, "node {id} is read by both queries");
            }
        }

        let node = candidate
            .dag
            .nodes
            .iter_mut()
            .find(|n| n.id == inner)
            .unwrap();
        node.materialization = Materialization::MatAtQuery(asap_types::ir::Medium::Memory);
        let stored = price(&candidate.dag, &every_10s(2), &data(), &calibration).unwrap();
        for (id, c) in &stored.per_node {
            assert_eq!(c.multiplier, 1, "node {id} is computed once");
            let once = cost.per_node[id].cost / multipliers[id] as f64;
            assert!((c.cost - once).abs() <= 1e-12 * once, "node {id}");
        }
        assert!(stored.total < cost.total);
    }

    /// Every combination of Pass 1 candidates of one query, as Stage 2
    /// candidates `P1`, `P2`, ...
    fn all_candidates(query: &str, target: &AccuracyTarget) -> Vec<PhysicalCandidate> {
        let root = lower_promql(query, target.clone());
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let mut choices = vec![vec![]];
        for target in &inventory.targets {
            choices = choices
                .into_iter()
                .flat_map(|choice: Vec<usize>| {
                    (0..target.choices.candidates.len()).map(move |index| {
                        let mut choice = choice.clone();
                        choice.push(index);
                        choice
                    })
                })
                .collect();
        }
        choices
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                let roots: Vec<_> = crate::per_computation_candidates::compose_logical_candidate(
                    &inventory, choice,
                )
                .unwrap()
                .into_iter()
                .map(|(_, root)| root)
                .collect();
                let mut candidate = optimize_physical_asap("L", &roots).unwrap();
                candidate.id = format!("P{}", index + 1);
                candidate
            })
            .collect()
    }

    fn has_summary_estimate(candidate: &PhysicalCandidate) -> bool {
        candidate
            .dag
            .nodes
            .iter()
            .any(|n| matches!(n.payload, Payload::ASAP(ASAPOp::SummaryEstimate { .. })))
    }

    /// Deployment evidence that every output was measured exact.
    struct EveryOutputMeasuredExact;

    impl AccuracyEvidenceProvider for EveryOutputMeasuredExact {
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<AccuracyGuarantee> {
            Some(AccuracyGuarantee::exact("measured"))
        }
    }

    /// A difference of two quantile estimates has no output guarantee, though
    /// each estimate is certified: its summary candidates stay listed as
    /// invalid with a reason. The deployment's empirical accuracy for the
    /// output admits them.
    #[test]
    fn combined_estimates_need_evidence_about_the_output() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let candidates = all_candidates(
            "quantile_over_time(0.99, m[1m]) - quantile_over_time(0.5, m[1m])",
            &target,
        );
        let summaries: Vec<_> = candidates
            .iter()
            .filter(|c| has_summary_estimate(c))
            .map(|c| c.id.as_str())
            .collect();
        assert!(!summaries.is_empty());

        let builtin = select_plan(
            &candidates,
            &[Some(target.clone())],
            &every_10s(1),
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        for id in &summaries {
            let rejection = builtin.rejected.iter().find(|r| r.id == *id).unwrap();
            assert!(!rejection.valid);
            assert!(
                rejection.reason.contains("no guarantee"),
                "{}",
                rejection.reason
            );
        }
        assert!(!summaries.contains(&builtin.selected.as_str()));

        let measured = select_plan(
            &candidates,
            &[Some(target)],
            &every_10s(1),
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for id in &summaries {
            assert!(measured.costs.contains_key(*id), "{id} is valid");
        }
    }

    /// `candidates` with every state node (a summary build or an exact
    /// aggregate) kept at query time, so selection charges its kept bytes.
    fn keeping_state(mut candidates: Vec<PhysicalCandidate>) -> Vec<PhysicalCandidate> {
        for candidate in &mut candidates {
            for node in &mut candidate.dag.nodes {
                if matches!(
                    node.payload,
                    Payload::ASAP(ASAPOp::SummaryAgg { .. })
                        | Payload::NonASAP(NonASAPOp::Aggregate { .. })
                ) {
                    node.materialization =
                        Materialization::MatAtQuery(asap_types::ir::Medium::Memory);
                }
            }
        }
        candidates
    }

    /// Kept exact quantile state keeps every value of its group: over one
    /// group of 10 000 values it costs more than a quantile sketch sized for
    /// ε = 0.01, δ = 0.01, which is selected; over 100 groups of 100 values it
    /// is smaller than the sketch and is selected. (State held only while a
    /// query runs is not charged.)
    #[test]
    fn exact_quantile_state_costs_more_than_a_sketch_over_large_groups() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        for (query, sketch_wins) in [
            ("quantile(0.99, rate(x[5m]))", true),
            ("quantile by (job) (0.99, rate(x[5m]))", false),
        ] {
            let candidates = keeping_state(all_candidates(query, &target));
            let selection = select_plan(
                &candidates,
                &[Some(target.clone())],
                &every_10s(1),
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap();
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            assert_eq!(
                has_summary_estimate(selected),
                sketch_wins,
                "{query}: {selection:?}"
            );
        }
    }

    /// Exact L2 state keeps one counter per distinct value of its group, so
    /// it is priced like exact quantile state, which keeps every value.
    #[test]
    fn exact_l2_state_is_priced_like_exact_quantile_state() {
        let aggregate_cost = |query: &str| {
            let candidates = all_candidates(query, &AccuracyTarget::Exact);
            let selection = select_plan(
                &candidates,
                &[None],
                &every_10s(1),
                &data(),
                PlanningModels::builtin(),
            )
            .unwrap();
            let exact = candidates
                .iter()
                .find(|c| !has_summary_estimate(c))
                .unwrap();
            selection.costs[&exact.id]
                .per_node
                .values()
                .find(|n| n.detail.starts_with("hash aggregate"))
                .unwrap()
                .cost
        };
        assert_eq!(
            aggregate_cost("l2_over_time(x[5m])"),
            aggregate_cost("quantile_over_time(0.99, x[5m])")
        );
    }

    /// A DDSketch is sized for values over nine decades: 1,037 bins at
    /// α = 0.01, 8 bytes each.
    #[test]
    fn ddsketch_bins_span_nine_decades() {
        assert_eq!(ddsketch_bins(0.01), 1_037);
        assert_eq!(
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::DDSketch,
                    SketchParams::DDSketch { alpha: 0.01 }
                ),
                GroupingStrategy::default(),
            ))
            .1,
            8 * 1_037
        );
    }

    /// DDSketch state grows as its relative accuracy α tightens.
    #[test]
    fn ddsketch_state_grows_as_alpha_shrinks() {
        let bytes = |alpha| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(SketchAlgorithm::DDSketch, SketchParams::DDSketch { alpha }),
                GroupingStrategy::default(),
            ))
            .1
        };
        assert!(
            bytes(0.1) < bytes(0.01),
            "{} vs {}",
            bytes(0.1),
            bytes(0.01)
        );
        assert!(
            bytes(0.01) < bytes(0.001),
            "{} vs {}",
            bytes(0.01),
            bytes(0.001)
        );
    }

    /// Every sketch family's state is sized from its parameters, as
    /// `asap_sketchlib` allocates it.
    #[test]
    fn summary_state_is_sized_from_sketch_params() {
        let bytes = |algorithm, params| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(algorithm, params),
                GroupingStrategy::default(),
            ))
            .1
        };
        assert_eq!(
            bytes(SketchAlgorithm::Hll, SketchParams::Hll { precision: 14 }),
            1 << 14
        );
        assert_eq!(
            bytes(SketchAlgorithm::Kmv, SketchParams::Kmv { k: 1_000 }),
            8 * 1_000
        );
        assert_eq!(
            bytes(
                SketchAlgorithm::UnivMon,
                SketchParams::UnivMon {
                    heap_size: 256,
                    sketch_rows: 5,
                    sketch_cols: 1_024,
                    layers: 16,
                }
            ),
            16 * (8 * 5 * 1_024 + 24 * 256)
        );
    }

    /// KLL state is the item array `asap_sketchlib` allocates up front:
    /// `compute_max_capacity(k, 8)` 8-byte items, with `k` capped at 26 602.
    #[test]
    fn kll_state_is_sized_as_asap_sketchlib_allocates_it() {
        let bytes = |k| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::Kll,
                    SketchParams::Kll {
                        k,
                        failure_probability: None,
                    },
                ),
                GroupingStrategy::default(),
            ))
            .1
        };
        assert_eq!(bytes(269), 8 * 1_206);
        assert_eq!(bytes(26_602), 8 * 80_118);
        assert_eq!(bytes(30_000), 8 * 80_118);
    }

    /// A UnivMon row updates layers `0..=b`, where its bottom layer `b` is
    /// geometric (`asap_sketchlib`'s `bottom_layer_for_hash`), so
    /// `2 - 2^(1 - layers)` layers in expectation; each updates one counter
    /// per sketch row and its heap.
    #[test]
    fn univmon_update_cost_is_expected_layers_times_rows_and_heap() {
        let ops = |layers| {
            summary_shape(&FieldDataType::Sketch(
                SketchKind::new(
                    SketchAlgorithm::UnivMon,
                    SketchParams::UnivMon {
                        heap_size: 256,
                        sketch_rows: 5,
                        sketch_cols: 1_024,
                        layers,
                    },
                ),
                GroupingStrategy::default(),
            ))
            .0
        };
        assert_eq!(ops(1), 6.0);
        assert_eq!(ops(2), 1.5 * 6.0);
        assert_eq!(ops(16), (2.0 - 2f64.powi(-15)) * 6.0);
    }

    /// Kept exact distinct-count state keeps every item of its group: over
    /// 10 000 groups of 300 rows it is smaller than a distinct-count sketch
    /// sized for ε = 0.01 and is selected; over one group of 3 000 000 rows a
    /// sketch is.
    #[test]
    fn exact_distinct_count_beats_a_sketch_over_small_groups() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.01,
        };
        let one_series = DataWorkload {
            input_cardinality: Evidence {
                value: Some(1),
                ..Default::default()
            },
            ..data()
        };
        for (data, sketch_wins) in [(data(), false), (one_series, true)] {
            let candidates = keeping_state(all_candidates("distinct_over_time(x[5m])", &target));
            let selection = select_plan(
                &candidates,
                &[Some(target.clone())],
                &every_10s(1),
                &data,
                PlanningModels::builtin(),
            )
            .unwrap();
            let selected = candidates
                .iter()
                .find(|c| c.id == selection.selected)
                .unwrap();
            assert_eq!(has_summary_estimate(selected), sketch_wins, "{selection:?}");
        }
    }

    /// A candidate that ties the selected one is rejected for losing on
    /// enumeration order, not as costlier.
    #[test]
    fn a_tie_is_reported_as_lost_on_enumeration_order() {
        let mut candidates = candidates();
        candidates.truncate(1);
        let mut twin = candidates[0].clone();
        twin.id = "P1-twin".into();
        candidates.push(twin);
        let selection = select_plan(
            &candidates,
            &[None],
            &every_10s(1),
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, "P1");
        let reason = &selection.rejected[0].reason;
        assert!(
            reason.contains("tied with the selected candidate P1")
                && reason.contains("enumeration order"),
            "{reason}"
        );
    }

    /// At δ = 0.001 a KLL candidate meets the target and is valid.
    #[test]
    fn kll_candidate_meets_a_failure_probability_below_one_percent() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let candidates = all_candidates("quantile(0.99, rate(x[5m]))", &target);
        let selection = select_plan(
            &candidates,
            &[Some(target)],
            &every_10s(1),
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let kll = candidates
            .iter()
            .find(|c| {
                c.dag.nodes.iter().any(|n| {
                    matches!(&n.payload, Payload::ASAP(ASAPOp::SummaryAgg {
                        family: FieldDataType::Sketch(kind, _), ..
                    }) if *kind.algorithm() == SketchAlgorithm::Kll)
                })
            })
            .unwrap();
        assert!(selection.costs.contains_key(&kll.id), "{selection:?}");
    }

    /// `scalar(v)` inherits `v`'s guarantee; arithmetic over it composes
    /// (`c · x` scales an absolute bound, `x − y` adds them); a rank error
    /// does not compose (a rank interval counts as none); one over exact
    /// operands only is exact.
    #[test]
    fn scalar_root_guarantees_compose_through_arithmetic() {
        let node = lower_promql("sum(m)", AccuracyTarget::Exact);
        let approximate = AccuracyGuarantee {
            metric: ErrorMetric::AbsoluteValue,
            bound: BoundExpr::Constant { value: 0.01 },
            failure_probability: ProbabilityExpr::Constant { value: 0.001 },
            provenance: vec![],
        };
        let with = |guarantee: Option<AccuracyGuarantee>| {
            ScalarExpr::PromqlScalarFromVector(Rc::new((*node).clone().with_guarantee(guarantee)))
        };
        let doubled = |operand: ScalarExpr| {
            QueryRoot::Scalar(ScalarExpr::Arithmetic {
                op: ArithmeticOpKind::Mul,
                left: Box::new(operand),
                right: Box::new(ScalarExpr::literal_f64(2.0)),
                semantics: ExprSemantics::Promql,
            })
        };
        assert_eq!(
            root_guarantee(&QueryRoot::Scalar(with(Some(approximate.clone())))),
            Some(approximate.clone())
        );
        let scaled = root_guarantee(&doubled(with(Some(approximate.clone())))).unwrap();
        assert_eq!(scaled.metric, ErrorMetric::AbsoluteValue);
        assert!((scaled.bound.evaluate().unwrap() - 0.02).abs() < 1e-12);
        assert_eq!(scaled.failure_probability.evaluate(), Some(0.001));
        let difference = QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op: ArithmeticOpKind::Sub,
            left: Box::new(with(Some(approximate.clone()))),
            right: Box::new(with(Some(approximate.clone()))),
            semantics: ExprSemantics::Promql,
        });
        let sum = root_guarantee(&difference).unwrap();
        assert!((sum.bound.evaluate().unwrap() - 0.02).abs() < 1e-12);
        let rank = AccuracyGuarantee {
            metric: ErrorMetric::Rank,
            ..approximate
        };
        assert_eq!(root_guarantee(&doubled(with(Some(rank)))), None);
        assert!(
            root_guarantee(&doubled(with(Some(AccuracyGuarantee::exact("exact")))))
                .is_some_and(|g| g.is_exact())
        );
    }

    /// Every candidate of the Stage 1 to Stage 3 run of one query.
    fn run(
        query: &str,
        target: &AccuracyTarget,
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> (Vec<PhysicalCandidate>, Selection) {
        run_selecting(query, target, target, data, models)
    }

    /// As [`run`], with summaries sized for `planned` and outputs checked
    /// against `selected`.
    fn run_selecting(
        query: &str,
        planned: &AccuracyTarget,
        selected: &AccuracyTarget,
        data: &DataWorkload,
        models: PlanningModels<'_>,
    ) -> (Vec<PhysicalCandidate>, Selection) {
        let root = lower_promql(query, planned.clone());
        let run = crate::pass::run_stage1_to_stage3(
            vec![(0, QueryRoot::Operator(root))],
            &[Some(selected.clone())],
            &[QueryRecurrence::Unknown],
            data,
            models,
        )
        .unwrap();
        (
            run.stage2_physical_asap_optimization,
            run.stage3_plan_selection.unwrap(),
        )
    }

    fn sketches(candidate: &PhysicalCandidate) -> Vec<SketchAlgorithm> {
        candidate
            .dag
            .nodes
            .iter()
            .filter_map(|n| match &n.payload {
                Payload::ASAP(ASAPOp::SummaryAgg {
                    family: FieldDataType::Sketch(kind, _),
                    ..
                }) => Some(kind.algorithm().clone()),
                _ => None,
            })
            .collect()
    }

    fn reads_only(candidate: &PhysicalCandidate, algorithm: SketchAlgorithm) -> bool {
        let sketches = sketches(candidate);
        !sketches.is_empty() && sketches.iter().all(|a| *a == algorithm)
    }

    fn output_guarantee(candidate: &PhysicalCandidate) -> Option<AccuracyGuarantee> {
        root_guarantee(&candidate.roots[0])
    }

    /// `max by (job)` over per-series DDSketch quantiles keeps their relative
    /// bound, and `2 · x` keeps it too (scaling preserves a relative error);
    /// so those candidates meet the target. Over KLL (a rank error) the
    /// result is a rank interval: no guarantee in the MVP.
    #[test]
    fn extremum_and_scaling_over_per_series_estimates_compose() {
        let target = AccuracyTarget::Epsilon(0.01);
        for query in [
            "max by (job) (quantile_over_time(0.99, m[1m]))",
            "2 * max by (job) (quantile_over_time(0.99, m[1m]))",
        ] {
            let (candidates, selection) = run(query, &target, &data(), PlanningModels::builtin());
            let ddsketch: Vec<_> = candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .collect();
            assert!(!ddsketch.is_empty(), "{query}");
            for candidate in ddsketch {
                let g = output_guarantee(candidate).expect(query);
                assert_eq!(g.metric, ErrorMetric::RelativeValue, "{query}");
                assert!(g.bound.evaluate().unwrap() <= 0.01, "{query}");
                assert!(selection.costs.contains_key(&candidate.id), "{query}");
            }
            for candidate in candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::Kll))
            {
                assert_eq!(output_guarantee(candidate), None, "{query}");
            }
        }
    }

    /// Negation, `abs` and `clamp` keep a DDSketch quantile's relative bound,
    /// so those candidates are selectable; a shift by a constant, a
    /// comparison (filter or `bool`) over the estimate compose to no
    /// guarantee in the MVP, so those are not.
    #[test]
    fn lipschitz_functions_keep_and_shifts_and_comparisons_drop_a_relative_bound() {
        let target = AccuracyTarget::Epsilon(0.01);
        let q = "quantile_over_time(0.99, m[1m])";
        for (query, kept) in [
            (format!("-{q}"), true),
            (format!("abs({q})"), true),
            (format!("clamp({q}, 1, 100)"), true),
            (format!("clamp_max({q}, 100)"), true),
            (format!("{q} + 1"), false),
            (format!("{q} > 5"), false),
            (format!("{q} > bool 5"), false),
        ] {
            let (candidates, selection) = run(&query, &target, &data(), PlanningModels::builtin());
            let ddsketch: Vec<_> = candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .collect();
            assert!(!ddsketch.is_empty(), "{query}");
            for candidate in ddsketch {
                let g = output_guarantee(candidate);
                if kept {
                    let g = g.expect(&query);
                    assert_eq!(g.metric, ErrorMetric::RelativeValue, "{query}");
                    assert!(g.bound.evaluate().unwrap() <= 0.01, "{query}");
                    assert!(selection.costs.contains_key(&candidate.id), "{query}");
                } else {
                    assert_eq!(g, None, "{query}");
                    assert!(!selection.costs.contains_key(&candidate.id), "{query}");
                }
            }
        }
    }

    /// A time-window fold over a subquery of estimates folds one estimate
    /// per subquery step and series, so its failure probability is scaled
    /// by the step count `⌈range / resolution⌉` (here 10), not by the input
    /// series count. Without an explicit resolution the step count is not in
    /// the query, and the fold has no guarantee.
    #[test]
    fn a_fold_over_subquery_estimates_counts_its_steps() {
        let target = AccuracyTarget::Epsilon(0.01);
        let q = "quantile_over_time(0.99, m[1m])";
        let (candidates, selection) = run(
            &format!("max_over_time({q}[10m:1m])"),
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        for candidate in ddsketch {
            let g = output_guarantee(candidate).expect("a guarantee");
            assert_eq!(g.metric, ErrorMetric::RelativeValue);
            assert!(g.bound.evaluate().unwrap() <= 0.01);
            let ProbabilityExpr::Scaled { count, .. } = &g.failure_probability else {
                panic!("{g:?}");
            };
            assert_eq!(count, &BoundExpr::Constant { value: 10.0 });
            assert!(selection.costs.contains_key(&candidate.id));
        }
        let (candidates, _) = run(
            &format!("max_over_time({q}[10m:])"),
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        for candidate in candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
        {
            assert_eq!(output_guarantee(candidate), None);
        }
    }

    /// Counting estimates by a label keeps the count exact: the group keys
    /// are labels, not the estimates (#669).
    #[test]
    fn counting_estimates_by_a_label_stays_exact() {
        let target = AccuracyTarget::Epsilon(0.01);
        let (candidates, _) = run(
            "count by (le) (quantile_over_time(0.99, m[1m]))",
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        // The inner quantile is a sketch; the count itself is exact.
        let sketched: Vec<_> = candidates
            .iter()
            .filter(|c| {
                let sketches = sketches(c);
                !sketches.is_empty()
                    && sketches
                        .iter()
                        .all(|a| matches!(a, SketchAlgorithm::Kll | SketchAlgorithm::DDSketch))
            })
            .collect();
        assert!(!sketched.is_empty());
        for candidate in sketched {
            assert!(
                output_guarantee(candidate).is_some_and(|g| g.is_exact()),
                "{:?}",
                sketches(candidate)
            );
        }
    }

    /// `avg by (job)` over per-series DDSketch quantiles has a relative bound
    /// that needs the values' sign, a statistic no input supplies: it stays
    /// symbolic and plan selection treats it as unmet.
    #[test]
    fn average_over_relative_estimates_needs_the_sign() {
        let target = AccuracyTarget::Epsilon(0.01);
        let (candidates, selection) = run(
            "avg by (job) (quantile_over_time(0.99, m[1m]))",
            &target,
            &data(),
            PlanningModels::builtin(),
        );
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        for candidate in ddsketch {
            let g = output_guarantee(candidate).expect("a symbolic bound");
            assert!(g.bound.has_unknown(), "{g:?}");
            assert!(!selection.costs.contains_key(&candidate.id));
        }
    }

    /// `max` folds one value per series, so its failure probability is
    /// scaled by the symbolic input row count. Stage 3 binds it from the data
    /// workload's input cardinality; without one, the target is unmet.
    #[test]
    fn stage3_binds_the_input_row_count_from_the_data_workload() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let query = "max by (job) (quantile_over_time(0.99, m[1m]))";
        let ddsketch = |candidates: &[PhysicalCandidate]| -> Vec<String> {
            candidates
                .iter()
                .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
                .map(|c| c.id.clone())
                .collect()
        };
        let (candidates, with) = run(query, &target, &data(), PlanningModels::builtin());
        let ids = ddsketch(&candidates);
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| with.costs.contains_key(id)));
        let unknown = DataWorkload {
            input_cardinality: Evidence::default(),
            ..data()
        };
        let (_, without) = run(query, &target, &unknown, PlanningModels::builtin());
        assert!(ids.iter().all(|id| !without.costs.contains_key(id)));
    }

    /// Deployment contract: every quantile operand lies in `[1, 1000]`.
    struct BoundedQuantiles;

    impl AccuracyEvidenceProvider for BoundedQuantiles {
        fn operand_domain(
            &self,
            _operand: &OperatorNode,
        ) -> Option<crate::accuracy::OperandDomain> {
            Some(crate::accuracy::OperandDomain {
                lower: 1.0,
                upper: 1_000.0,
                max_samples: 1_000_000,
                contract: "test source enforces [1, 1000]".into(),
            })
        }
    }

    /// A ratio of two DDSketch quantiles has a relative bound only with an
    /// enforced operand domain: Stage 3 binds it from the deployment's
    /// `OperandDomain` contract; without one the target is unmet. (A
    /// DDSketch quantile over an exact one keeps its relative bound.) Each
    /// operand is sized for the entry's target, so the ratio's bound, about
    /// twice that, is checked against a looser one.
    #[test]
    fn ddsketch_ratio_meets_its_target_only_with_operand_domains() {
        let planned = AccuracyTarget::Epsilon(0.01);
        let target = AccuracyTarget::Epsilon(0.05);
        let query = "quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])";
        let run = |models| run_selecting(query, &planned, &target, &data(), models);
        let (candidates, without) = run(PlanningModels::builtin());
        let ids: Vec<_> = candidates
            .iter()
            .filter(|c| sketches(c) == [SketchAlgorithm::DDSketch, SketchAlgorithm::DDSketch])
            .map(|c| c.id.clone())
            .collect();
        // The Pass 1 candidate and its Pass 2 candidates sharing the input.
        assert!(!ids.is_empty());
        assert!(ids.iter().all(|id| !without.costs.contains_key(id)));
        let (_, with) = run(PlanningModels::builtin().with_evidence(&BoundedQuantiles));
        assert!(ids.iter().all(|id| with.costs.contains_key(id)), "{with:?}");
    }

    /// `query` carrying `guarantee`, as a `scalar(…)` operand.
    fn scalar_of(query: &str, guarantee: AccuracyGuarantee) -> ScalarExpr {
        let node = lower_promql(query, AccuracyTarget::Exact);
        ScalarExpr::PromqlScalarFromVector(Rc::new((*node).clone().with_guarantee(Some(guarantee))))
    }

    fn guarantee(metric: ErrorMetric, bound: f64, delta: f64) -> AccuracyGuarantee {
        AccuracyGuarantee {
            metric,
            bound: BoundExpr::Constant { value: bound },
            failure_probability: ProbabilityExpr::Constant { value: delta },
            provenance: vec![],
        }
    }

    fn scalar_root(op: ArithmeticOpKind, left: ScalarExpr, right: ScalarExpr) -> QueryRoot {
        QueryRoot::Scalar(ScalarExpr::Arithmetic {
            op,
            left: Box::new(left),
            right: Box::new(right),
            semantics: ExprSemantics::Promql,
        })
    }

    fn norms(sources: &[(&str, f64)]) -> DataWorkload {
        DataWorkload {
            stream_norms: Evidence {
                value: Some(
                    sources
                        .iter()
                        .map(|(source, l1)| {
                            (
                                source.to_string(),
                                asap_types::workload::StreamNorms {
                                    l1: Some(*l1),
                                    l2: None,
                                },
                            )
                        })
                        .collect(),
                ),
                ..Evidence::default()
            },
            ..data()
        }
    }

    /// A difference of two Count-Min estimates of the raw values of
    /// different metrics is bound by each metric's own stream norm from the
    /// data workload
    /// (`0.01 · 100 + 0.02 · 1000`); a metric without a norm leaves the
    /// bound unmet.
    #[test]
    fn stage3_binds_each_metric_stream_norm() {
        // Count-Min summaries of each metric's raw value column.
        let raw = |query: &str, g: AccuracyGuarantee| {
            ScalarExpr::PromqlScalarFromVector(crate::test_support::estimate_over(
                crate::test_support::weighted(
                    crate::test_support::cms_summary(query),
                    asap_types::post_asap::SummaryInputExpr::Column(
                        asap_types::pre_asap::ColumnRef::SampleValue,
                    ),
                ),
                g,
            ))
        };
        let root = scalar_root(
            ArithmeticOpKind::Sub,
            raw(
                "count by (job) (a)",
                guarantee(ErrorMetric::Frequency, 0.01, 0.001),
            ),
            raw(
                "count by (job) (b)",
                guarantee(ErrorMetric::Frequency, 0.02, 0.001),
            ),
        );
        let models = PlanningModels::builtin();
        let both =
            bound_root_guarantee(&root, &norms(&[("a", 100.0), ("b", 1_000.0)]), &models).unwrap();
        assert!(
            (both.bound.evaluate().unwrap() - 21.0).abs() < 1e-9,
            "{both:?}"
        );
        let one = bound_root_guarantee(&root, &norms(&[("a", 100.0)]), &models).unwrap();
        assert!(one.bound.has_unknown());
    }

    /// A stream norm is bound only for a summary of the source's raw value
    /// column: a count over the same source (constant weight) needs the
    /// source's row count, which nothing binds, and a summary of a computed
    /// value (`m * 1000`) needs a norm nothing binds; both stay unmet.
    #[test]
    fn stage3_binds_a_stream_norm_only_for_the_raw_value_column() {
        use asap_types::post_asap::SummaryInputExpr;
        use asap_types::pre_asap::ColumnRef;
        let frequency = guarantee(ErrorMetric::Frequency, 0.01, 0.001);
        let doubled = |summary: Rc<OperatorNode>| {
            scalar_root(
                ArithmeticOpKind::Mul,
                ScalarExpr::literal_f64(2.0),
                ScalarExpr::PromqlScalarFromVector(crate::test_support::estimate_over(
                    summary,
                    frequency.clone(),
                )),
            )
        };
        let models = PlanningModels::builtin();
        let data = norms(&[("m", 100.0)]);
        let column = || SummaryInputExpr::Column(ColumnRef::SampleValue);
        let raw = doubled(crate::test_support::weighted(
            crate::test_support::cms_summary("count by (job) (m)"),
            column(),
        ));
        let g = bound_root_guarantee(&raw, &data, &models).unwrap();
        assert!((g.bound.evaluate().unwrap() - 2.0).abs() < 1e-12, "{g:?}");
        for unbound in [
            doubled(Rc::new(crate::test_support::cms_summary(
                "count by (job) (m)",
            ))),
            doubled(crate::test_support::weighted(
                crate::test_support::cms_summary("count by (job) (m * 1000)"),
                column(),
            )),
        ] {
            let g = bound_root_guarantee(&unbound, &data, &models).unwrap();
            assert!(g.bound.has_unknown(), "{g:?}");
        }
    }

    /// A product and a quotient of two estimates are bound from the
    /// deployment's operand domains (`[1, 1000]`):
    /// `|x|·e_y + |y|·e_x + e_x·e_y` and `(|x|·e_y + |y|·e_x) / (|y|·(|y| − e_y))`
    /// at the domain's extremes; without domains they stay unmet. A ratio of
    /// relative errors with no division operator to bind stays unmet too.
    #[test]
    fn stage3_binds_products_and_quotients_from_operand_domains() {
        let (x, y) = (
            scalar_of("sum(a)", guarantee(ErrorMetric::AbsoluteValue, 0.01, 0.001)),
            scalar_of("sum(b)", guarantee(ErrorMetric::AbsoluteValue, 0.02, 0.001)),
        );
        let product = scalar_root(ArithmeticOpKind::Mul, x.clone(), y.clone());
        let quotient = scalar_root(ArithmeticOpKind::Div, x, y);
        let bounded = PlanningModels::builtin().with_evidence(&BoundedQuantiles);
        for (root, expected) in [
            (&product, 1_000.0 * 0.02 + 1_000.0 * 0.01 + 0.01 * 0.02),
            (&quotient, (1_000.0 * 0.02 + 1_000.0 * 0.01) / (1.0 - 0.02)),
        ] {
            let g = bound_root_guarantee(root, &data(), &bounded).unwrap();
            assert!(
                (g.bound.evaluate().unwrap() - expected).abs() < 1e-9,
                "{g:?}"
            );
            assert!((g.failure_probability.evaluate().unwrap() - 0.002).abs() < 1e-12);
            let unbound = bound_root_guarantee(root, &data(), &PlanningModels::builtin()).unwrap();
            assert!(unbound.bound.has_unknown(), "{unbound:?}");
        }
        let ratio = scalar_root(
            ArithmeticOpKind::Div,
            scalar_of("sum(a)", guarantee(ErrorMetric::RelativeValue, 0.01, 0.0)),
            scalar_of("sum(b)", guarantee(ErrorMetric::RelativeValue, 0.01, 0.0)),
        );
        let g = bound_root_guarantee(&ratio, &data(), &PlanningModels::builtin()).unwrap();
        assert!(g.bound.has_unknown(), "{g:?}");
    }

    /// A ratio divided by an exact vector, `(q99 / q50) / max_over_time(m)`,
    /// keeps the inner ratio's symbolic relative bound; Stage 3 binds it at
    /// the division of the two estimates, not at the outer division that
    /// inherited it, so the candidate is selectable.
    #[test]
    fn a_scaled_ratio_binds_at_its_division() {
        let planned = AccuracyTarget::Epsilon(0.01);
        let target = AccuracyTarget::Epsilon(0.05);
        let query = "(quantile_over_time(0.99, m[1m]) / quantile_over_time(0.5, m[1m])) \
                     / max_over_time(m[1m])";
        let (candidates, selection) = run_selecting(
            query,
            &planned,
            &target,
            &data(),
            PlanningModels::builtin().with_evidence(&BoundedQuantiles),
        );
        let ids: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .map(|c| c.id.clone())
            .collect();
        assert!(!ids.is_empty());
        assert!(
            ids.iter().all(|id| selection.costs.contains_key(id)),
            "{selection:?}"
        );
    }

    /// The input row count is bound only from fresh evidence: Stage 3 has
    /// no planning time, so evidence with a validity window (which could
    /// have expired) is not used; timeless declared evidence is.
    #[test]
    fn stage3_binds_only_fresh_input_cardinality() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let query = "max by (job) (quantile_over_time(0.99, m[1m]))";
        let expiring = DataWorkload {
            input_cardinality: Evidence {
                value: Some(10_000),
                observed_at_ms: Some(1_000),
                valid_for_ms: Some(1_000),
                ..Evidence::default()
            },
            ..data()
        };
        let (candidates, selection) = run(query, &target, &expiring, PlanningModels::builtin());
        let ddsketch: Vec<_> = candidates
            .iter()
            .filter(|c| reads_only(c, SketchAlgorithm::DDSketch))
            .collect();
        assert!(!ddsketch.is_empty());
        assert!(ddsketch
            .iter()
            .all(|c| !selection.costs.contains_key(&c.id)));
    }

    /// `quantile_over_time(0.99, m[1m])` with its KLL candidate chosen, its
    /// state materialized as `materialization`.
    fn kll_dag(materialization: Materialization) -> PhysicalASAPDAG {
        use asap_types::ir::{
            apply_materialization_timings, MaterializationAssignment, TimingMemo,
        };
        let root = lower_promql(
            "quantile_over_time(0.99, m[1m])",
            AccuracyTarget::EpsilonDelta {
                epsilon: 0.01,
                delta: 0.01,
            },
        );
        let inventory = crate::per_computation_candidates::generate_per_computation_candidates(
            vec![(0, QueryRoot::Operator(root))],
        )
        .unwrap();
        let choice: Vec<_> = inventory
            .targets
            .iter()
            .map(|t| {
                t.choices
                    .candidates
                    .iter()
                    .position(|c| {
                        *c == crate::per_computation_candidates::ComputationCandidate::Summary(
                            SketchAlgorithm::Kll,
                        )
                    })
                    .unwrap_or(0)
            })
            .collect();
        let roots =
            crate::per_computation_candidates::compose_logical_candidate(&inventory, &choice)
                .unwrap();
        let QueryRoot::Operator(root) = &roots[0].1 else {
            panic!("operator root")
        };
        let state = OperatorNode::reachable(root)
            .into_iter()
            .find(|n| matches!(n.asap(), Some(ASAPOp::SummaryAgg { .. })))
            .expect("a KLL state");
        let mut assignment =
            MaterializationAssignment::with_default(Materialization::NotMaterialized);
        assignment.set(&state, materialization);
        let timed =
            apply_materialization_timings(root, &assignment, &mut TimingMemo::new()).unwrap();
        asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids(&[
            QueryRoot::Operator(timed),
        ])
        .unwrap()
        .dag
    }

    const AT_INGESTION: Materialization =
        Materialization::MatAtIngestion(asap_types::ir::Medium::Memory);
    const AT_QUERY: Materialization = Materialization::MatAtQuery(asap_types::ir::Medium::Memory);

    fn node_of(dag: &PhysicalASAPDAG, pick: impl Fn(&Payload) -> bool) -> PhysicalASAPNodeId {
        dag.nodes.iter().find(|n| pick(&n.payload)).unwrap().id
    }

    fn is_scan(payload: &Payload) -> bool {
        matches!(payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
    }

    fn is_build(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))
    }

    fn priced(dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]) -> CandidateCost {
        price(dag, recurrence, &data(), &illustrative_calibration()).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1e-300)
    }

    /// An ingestion-time node is priced over one second of arriving rows,
    /// once per second, whatever its readers' evaluation rate; a query-time
    /// node scales with that rate.
    #[test]
    fn ingestion_time_node_is_priced_per_second_of_arriving_rows() {
        let dag = kll_dag(AT_INGESTION);
        let (fast, slow) = (
            priced(&dag, &[repeating(1_000)]),
            priced(&dag, &[repeating(10_000)]),
        );
        for id in [node_of(&dag, is_scan), node_of(&dag, is_build)] {
            assert_eq!(fast.per_node[&id].cost, slow.per_node[&id].cost);
            assert_eq!(fast.per_node[&id].per_second, 1.0);
        }
        let estimate = dag.roots[0];
        assert!(close(
            fast.per_node[&estimate].cost,
            10.0 * slow.per_node[&estimate].cost
        ));
    }

    /// Kept state is charged per byte per second: twice the state's bytes at
    /// ingestion time (the window being built and the completed one), once
    /// at query time; state computed and dropped within a query has none.
    #[test]
    fn kept_state_is_charged_per_byte_per_second() {
        let cost = |dag: &PhysicalASAPDAG, w| {
            let calibration = ResourceCalibration {
                cost_per_retained_byte_second: w,
                ..illustrative_calibration()
            };
            price(dag, &every_10s(1), &data(), &calibration).unwrap()
        };
        for (materialization, windows) in [(AT_INGESTION, 2.0), (AT_QUERY, 1.0)] {
            let dag = kll_dag(materialization);
            let build = node_of(&dag, is_build);
            let (with, without) = (cost(&dag, 1.25e-7), cost(&dag, 0.0));
            let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) =
                &dag.nodes.iter().find(|n| n.id == build).unwrap().payload
            else {
                unreachable!()
            };
            // One state per series: data() declares 10 000.
            let kept = windows * 10_000.0 * summary_shape(family).1 as f64;
            let term = with.per_node[&build].cost - without.per_node[&build].cost;
            assert!(close(term, 1.25e-7 * kept), "{materialization:?}: {term}");
            assert!(close(with.total - without.total, term));
        }
        let transient = kll_dag(Materialization::NotMaterialized);
        assert_eq!(cost(&transient, 1.25e-7).total, cost(&transient, 0.0).total);
    }

    /// Memory is charged only as kept bytes per second: the memory a
    /// computation holds while it runs (a sketch being built, an exact
    /// quantile's values) adds nothing, whatever the per-computation memory
    /// coefficient.
    #[test]
    fn memory_held_only_while_a_query_runs_is_not_charged() {
        for query in [
            "quantile_over_time(0.99, m[1m])",
            "quantile by (job) (0.99, m)",
        ] {
            let target = AccuracyTarget::Exact;
            let root = lower_promql(query, target);
            let dag = optimize_physical_asap("L", &[QueryRoot::Operator(root)])
                .unwrap()
                .dag;
            let total = |per_computation_memory| {
                let calibration = ResourceCalibration {
                    cost_per_retained_byte: per_computation_memory,
                    ..illustrative_calibration()
                };
                price(&dag, &every_10s(1), &data(), &calibration)
                    .unwrap()
                    .total
            };
            assert_eq!(total(0.0), total(1.0), "{query}");
        }
        for materialization in [AT_INGESTION, AT_QUERY, Materialization::NotMaterialized] {
            let dag = kll_dag(materialization);
            let total = |per_computation_memory| {
                let calibration = ResourceCalibration {
                    cost_per_retained_byte: per_computation_memory,
                    ..illustrative_calibration()
                };
                price(&dag, &every_10s(1), &data(), &calibration)
                    .unwrap()
                    .total
            };
            assert_eq!(total(0.0), total(1.0), "{materialization:?}");
        }
    }

    /// Kept exact state that grows with the data is charged by the values it
    /// keeps: a materialized exact quantile keeps every value of its group,
    /// a sum one value per group.
    #[test]
    fn kept_exact_state_is_charged_by_the_values_it_keeps() {
        let kept = |query: &str| {
            let root = lower_promql(query, AccuracyTarget::Exact);
            let mut dag = optimize_physical_asap("L", &[QueryRoot::Operator(root)])
                .unwrap()
                .dag;
            let aggregate = dag
                .nodes
                .iter_mut()
                .find(|n| matches!(n.payload, Payload::NonASAP(NonASAPOp::Aggregate { .. })))
                .unwrap();
            aggregate.materialization = AT_QUERY;
            let id = aggregate.id;
            let cost = |w| {
                let calibration = ResourceCalibration {
                    cost_per_retained_byte_second: w,
                    ..illustrative_calibration()
                };
                price(&dag, &every_10s(1), &data(), &calibration)
                    .unwrap()
                    .per_node[&id]
                    .cost
            };
            (cost(1.0) - cost(0.0)).round() as u64
        };
        let (quantile, sum) = (
            kept("quantile by (job) (0.99, m)"),
            kept("sum by (job) (m)"),
        );
        assert!(quantile > 10 * sum, "{quantile} vs {sum}");
    }

    /// A `NotMaterialized` node read by two queries is computed for each, at
    /// each one's rate; `MatAtQuery`, it is computed once at their combined
    /// rate, where queries at the same interval run together.
    #[test]
    fn rates_follow_per_consumer_charging() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::Exact);
        let one = optimize_physical_asap("L", &[QueryRoot::Operator(root.clone())])
            .unwrap()
            .dag;
        let mut two = optimize_physical_asap(
            "L",
            &[QueryRoot::Operator(root.clone()), QueryRoot::Operator(root)],
        )
        .unwrap()
        .dag;
        let without_memory = ResourceCalibration {
            cost_per_retained_byte_second: 0.0,
            ..illustrative_calibration()
        };
        let total = |dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]| {
            price(dag, recurrence, &data(), &without_memory)
                .unwrap()
                .total
        };
        let single = total(&one, &every_10s(1));
        let mixed = [repeating(10_000), repeating(20_000)];
        assert!(close(total(&two, &every_10s(2)), 2.0 * single));
        assert!(close(total(&two, &mixed), 1.5 * single));
        assert_eq!(two.roots[0], two.roots[1], "one root node for both queries");
        for node in &mut two.nodes {
            node.materialization = AT_QUERY;
        }
        assert!(close(total(&two, &every_10s(2)), single));
        assert!(close(total(&two, &mixed), 1.5 * single));
    }

    /// One-off and unknown recurrence are spread over the horizon:
    /// invocations × per-evaluation cost / horizon_s.
    #[test]
    fn one_off_recurrence_is_spread_over_the_horizon() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::Exact);
        let dag = optimize_physical_asap("L", &[QueryRoot::Operator(root)])
            .unwrap()
            .dag;
        let horizon = illustrative_calibration().horizon_s;
        assert_eq!(horizon, 3_600.0);
        let per_evaluation = priced(&dag, &[repeating(1_000)]).total;
        let once = priced(
            &dag,
            &[QueryRecurrence::OneTime {
                invocations: 3,
                execute_at: None,
            }],
        )
        .total;
        assert!(close(once, 3.0 * per_evaluation / horizon), "{once}");
        assert!(close(
            priced(&dag, &[QueryRecurrence::Unknown]).total,
            per_evaluation / horizon
        ));
    }

    /// A calibration outside its domain is an error, not a price.
    #[test]
    fn invalid_calibration_is_rejected() {
        let dag = kll_dag(Materialization::NotMaterialized);
        for calibration in [
            ResourceCalibration {
                cost_per_retained_byte_second: -1.0,
                ..illustrative_calibration()
            },
            ResourceCalibration {
                horizon_s: 0.0,
                ..illustrative_calibration()
            },
        ] {
            assert!(price(&dag, &every_10s(1), &data(), &calibration).is_err());
        }
    }
}
