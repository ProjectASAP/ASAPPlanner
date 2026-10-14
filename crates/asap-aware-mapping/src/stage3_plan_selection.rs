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
//! A node is charged once per computation (#509): a materialized node
//! (`MatAtIngestion` or `MatAtQuery`) once, since its output is stored and
//! reused; a `NotMaterialized` node once per consumer that computes it, since
//! it is recomputed for each use and not stored. Its consumers are the
//! queries whose sub-DAG reaches it and the materialized nodes above it,
//! counted without looking through a materialized node: a node below one is
//! computed when that node is, so the materialized node counts as one
//! consumer. Stage 2 does not duplicate a shared node; the multiplier is
//! recorded on its cost ([`NodeCost::multiplier`]). Every node is
//! `NotMaterialized` until #604, so a shared node is charged once per query.
//!
//! Prices come from [`crate::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count; summary build and estimation are priced as rows × sketch depth and
//! groups × k. State is priced by its bytes: a sketch by its parameters, a
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
use std::collections::{BTreeMap, HashMap, HashSet};
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
    AccuracyGuarantee, CompositionOperator, ErrorMetric, FieldDataType, Materialization,
    SketchAlgorithm, SketchParams, SketchStatistic, WeightDomain,
};
use asap_types::pre_asap::{AggIntent, DataType, Reduction, ScalarValue, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::DataWorkload;
use thiserror::Error;

use crate::accuracy::composition::{
    parse_source_norm_statistic, DIVISION_OPERAND_DOMAINS, INPUT_ROW_COUNT,
    PRODUCT_OPERAND_DOMAINS, STREAM_L1_NORM,
};
use crate::accuracy::estimators::hll;
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

pub const COST_UNIT: &str = "cpu_ms_per_workload_evaluation";
pub const COST_SOURCE: &str = "analytical-cost-v1 (illustrative statistics)";

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;

/// `cost` is one computation's price times `multiplier`, the times the
/// node is computed per workload evaluation ([`computations`]).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    pub cost: f64,
    pub multiplier: u64,
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
    #[error("candidate {candidate} has {roots} roots but {targets} accuracy targets were given")]
    TargetCount {
        candidate: String,
        roots: usize,
        targets: usize,
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
/// target, price the rest and
/// select the cheapest (the first on ties). `targets[i]` is the requirement
/// of `candidate.roots[i]`; `None` imposes none.
pub fn select_plan(
    cands: &[PhysicalCandidate],
    targets: &[Option<AccuracyTarget>],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<Selection, SelectionError> {
    let mut costs = BTreeMap::new();
    let mut rejected = Vec::new();
    let mut best: Option<(&str, f64)> = None;
    for candidate in cands {
        if candidate.roots.len() != targets.len() {
            return Err(SelectionError::TargetCount {
                candidate: candidate.id.clone(),
                roots: candidate.roots.len(),
                targets: targets.len(),
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
        let cost = price(&candidate.dag, data).map_err(|(node, error)| SelectionError::Cost {
            candidate: candidate.id.clone(),
            node,
            error,
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
/// weights proven non-negative, which its bound assumes.
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
                && !matches!(input.weight_domain, WeightDomain::NonNegative { .. })
            {
                return Some(format!(
                    "q{query}: {} needs non-negative update weights, and these are not proven \
                     non-negative",
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
/// - classic HLL's failure probability, from the data workload's per-source
///   `max_distinct_per_evaluation`;
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
/// one source's stream norm, or classic HLL's failure probability given one
/// source's distinct bound, from evidence that holds at any planning time.
fn workload_statistic(statistic: &str, data: &DataWorkload) -> Option<f64> {
    // Without a planning time, only evidence that cannot expire is fresh.
    const ANY_TIME: u64 = u64::MAX;
    if statistic == INPUT_ROW_COUNT {
        return data
            .input_cardinality
            .value_at(ANY_TIME)
            .map(|rows| *rows as f64);
    }
    if let Some((precision, source)) = hll::parse_classic_failure_statistic(statistic) {
        let bound = data.max_distinct_per_evaluation.value_at(ANY_TIME)?;
        return Some(hll::classic_failure_probability(
            precision,
            *bound.get(source)?,
        ));
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

/// How many times each node of `dag` is computed per workload evaluation:
/// 1 for a materialized node; for a `NotMaterialized` one, the number of
/// consumers that reach it, where a consumer is a query (operator or scalar
/// root) or a materialized node, and a walk stops at the first materialized
/// node it meets. A node read several times within one query's walk counts
/// once for it.
fn computations(dag: &PhysicalASAPDAG) -> HashMap<PhysicalASAPNodeId, u64> {
    let materialized: HashSet<_> = dag
        .nodes
        .iter()
        .filter(|n| n.materialization != Materialization::NotMaterialized)
        .map(|n| n.id)
        .collect();
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    // Each consumer's starting nodes: what a query reads, or a materialized
    // node's inputs.
    let consumers = dag
        .roots
        .iter()
        .map(|root| vec![*root])
        .chain(
            dag.scalar_roots
                .iter()
                .map(|(_, expr)| expr.operator_refs().into_iter().copied().collect()),
        )
        .chain(
            dag.nodes
                .iter()
                .filter(|n| materialized.contains(&n.id))
                .map(|n| inputs(n.id).collect()),
        );
    let mut count: HashMap<_, u64> = HashMap::new();
    for start in consumers {
        let mut seen = HashSet::new();
        let mut stack: Vec<PhysicalASAPNodeId> = start;
        while let Some(id) = stack.pop() {
            if materialized.contains(&id) || !seen.insert(id) {
                continue;
            }
            *count.entry(id).or_default() += 1;
            stack.extend(inputs(id));
        }
    }
    dag.nodes
        .iter()
        .map(|n| {
            let times = if materialized.contains(&n.id) {
                1
            } else {
                count.get(&n.id).copied().unwrap_or(0)
            };
            (n.id, times)
        })
        .collect()
}

/// Price every node of `dag` once per computation ([`computations`]). Nodes
/// are exported children first, so each node's input statistics are known
/// when it is reached.
fn price(
    dag: &PhysicalASAPDAG,
    data: &DataWorkload,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
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
    let calibration = ResourceCalibration {
        cost_per_cpu_op: 1e-6,
        cost_per_scan_byte: 1e-7,
        cost_per_retained_byte: 1e-7,
        version: "illustrative-v1".into(),
    };
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let multipliers = computations(dag);
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
    for node in &dag.nodes {
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
            match reduction {
                Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => 1,
                Reduction::Reduce(keys) if !keys.is_without() => DEFAULT_GROUP_COUNT,
                _ => shape.series,
            }
            .min(input.rows.max(1))
        };
        let (out, estimate, detail) = match &node.payload {
            Payload::NonASAP(operator) => match operator {
                NonASAPOp::Scan { .. } => {
                    // A scan reads what its time range keeps.
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
        let cost = estimate
            .and_then(|estimate| estimate.calibrated_cost(&calibration))
            .map_err(|error| (node.id, error))?;
        let multiplier = multipliers[&node.id];
        output.insert(node.id, out);
        per_node.insert(
            node.id,
            NodeCost {
                cost: cost * multiplier as f64,
                multiplier,
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

    /// Exact (P1), CMS+heap (P2) and CountSketch+heap (P3) realizations of
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
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        let p2 = selection.rejected.iter().find(|r| r.id == "P2").unwrap();
        assert!(!p2.valid);
        assert!(p2.reason.contains("non-negative"), "{}", p2.reason);
    }

    /// A query without a target constrains nothing: the Count-Min
    /// candidate is admitted and priced.
    #[test]
    fn count_min_is_admitted_without_a_target() {
        let selection =
            select_plan(&candidates(), &[None], &data(), PlanningModels::builtin()).unwrap();
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
        let cost = price(&candidate.dag, &data()).unwrap();
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
        let stored = price(&candidate.dag, &data()).unwrap();
        for (id, c) in &stored.per_node {
            assert_eq!(c.multiplier, 1, "node {id} is computed once");
            assert_eq!(c.cost, cost.per_node[id].cost / multipliers[id] as f64);
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
            &data(),
            PlanningModels::builtin().with_evidence(&EveryOutputMeasuredExact),
        )
        .unwrap();
        for id in &summaries {
            assert!(measured.costs.contains_key(*id), "{id} is valid");
        }
    }

    /// Exact quantile state keeps every value of its group: over one group of
    /// 10 000 values it costs more than a quantile sketch sized for
    /// ε = 0.01, δ = 0.01, which is selected; over 100 groups of 100 values it
    /// is smaller than the sketch and is selected.
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
            let candidates = all_candidates(query, &target);
            let selection = select_plan(
                &candidates,
                &[Some(target.clone())],
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
            let selection =
                select_plan(&candidates, &[None], &data(), PlanningModels::builtin()).unwrap();
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

    /// Exact distinct-count state keeps every item of its group: over 10 000
    /// groups of 300 rows it is smaller than a distinct-count sketch sized for
    /// ε = 0.01 and is selected; over one group of 3 000 000 rows a sketch is.
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
            let candidates = all_candidates("distinct_over_time(x[5m])", &target);
            let selection = select_plan(
                &candidates,
                &[Some(target.clone())],
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
        let selection =
            select_plan(&candidates, &[None], &data(), PlanningModels::builtin()).unwrap();
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
}
