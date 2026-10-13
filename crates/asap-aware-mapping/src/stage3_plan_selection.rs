//! #509 Stage 3, plan selection (MVP): the only stage that computes cost.
//!
//! Each CandidatePhysicalASAPDAG from Stage 2 is checked against every
//! query's accuracy target: the query's output meets it when its root
//! guarantee does (theoretical: Stage 1 composes it with the built-in
//! accuracy model), or when the deployment's empirical evidence shows this
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
//! groups × k. These numbers are illustrative, not calibrated: a binary,
//! join or concat node is priced from its first input only (MVP; follow-up).
//! A cost error on any candidate aborts selection; it is reachable only from
//! bad declared rates. Latency bounds and deployment capabilities are not
//! checked yet.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, OperatorNode, QueryRoot, ScalarExpr};
use asap_types::post_asap::{
    ExecutionTiming, FieldDataType, Materialization, ResultGuarantee, SketchAlgorithm, SketchParams,
    SketchStatistic, WeightDomain,
};
use asap_types::pre_asap::{DataType, Reduction, Schema};
use asap_types::types::AccuracyTarget;
use asap_types::workload::{DataWorkload, QueryRecurrence, RepeatedDemand};
use thiserror::Error;

use crate::analytical_cost::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceCalibration, ResourceEstimate,
};
use crate::physical_operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::stage2_physical_asap_optimization::PhysicalCandidate;
use crate::PlanningModels;

/// Cost per second of wall time; one cost unit is one CPU-millisecond under
/// [`Stage3Calibration::ILLUSTRATIVE`].
pub const COST_UNIT: &str = "cost_per_second";
pub const COST_SOURCE: &str = "analytical-cost-v2 (illustrative statistics)";

/// Stage 3's price coefficients and amortization horizon, a deployment input
/// on [`PlanningModels`]. Values are illustrative until calibrated from
/// measurements.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stage3Calibration {
    pub cost_per_cpu_op: f64,
    pub cost_per_scan_byte: f64,
    /// Price of state retained across evaluations, per byte per second.
    pub cost_per_retained_byte_second: f64,
    /// Seconds over which one-off, scheduled and unknown recurrence is
    /// amortized.
    pub horizon_s: f64,
    pub version: &'static str,
}

impl Stage3Calibration {
    /// 1 ns of CPU per operation; 1 GB retained costs 1/8 vCPU (125 CPU-ms
    /// per second); one-off work is amortized over 1 h.
    pub const ILLUSTRATIVE: Self = Self {
        cost_per_cpu_op: 1e-6,
        cost_per_scan_byte: 1e-7,
        cost_per_retained_byte_second: 1.25e-7,
        horizon_s: 3_600.0,
        version: "illustrative-v2",
    };

    fn validate(&self) -> Result<(), AnalyticalCostError> {
        for (name, value) in [
            ("cost_per_cpu_op", self.cost_per_cpu_op),
            ("cost_per_scan_byte", self.cost_per_scan_byte),
            (
                "cost_per_retained_byte_second",
                self.cost_per_retained_byte_second,
            ),
        ] {
            if !value.is_finite() || value < 0.0 {
                return Err(AnalyticalCostError::InvalidCalibration(name, value));
            }
        }
        if !self.horizon_s.is_finite() || self.horizon_s <= 0.0 {
            return Err(AnalyticalCostError::InvalidCalibration(
                "horizon_s",
                self.horizon_s,
            ));
        }
        Ok(())
    }
}

impl Default for Stage3Calibration {
    fn default() -> Self {
        Self::ILLUSTRATIVE
    }
}

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;

/// `cost` is the node's price per second ([`price`]). `multiplier` is the
/// number of times the node is computed per evaluation of its consumers
/// ([`computations`]); `per_second` is how often it is computed per second.
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
/// `true`: it lost on cost.
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
        if let Some(reason) = accuracy_violation(candidate, targets, &models) {
            rejected.push(Rejection {
                id: candidate.id.clone(),
                valid: false,
                reason,
            });
            continue;
        }
        let cost = price(&candidate.dag, recurrence, data, &models.calibration).map_err(
            |(node, error)| SelectionError::Cost {
            candidate: candidate.id.clone(),
                node,
                error,
            },
        )?;
        if best.is_none_or(|(_, total)| cost.total < total) {
            best = Some((&candidate.id, cost.total));
        }
        costs.insert(candidate.id.clone(), cost);
    }
    let Some((selected, best_total)) = best else {
        return Err(SelectionError::NoValidCandidate(rejected));
    };
    for candidate in cands {
        if candidate.id != selected && costs.contains_key(&candidate.id) {
            rejected.push(Rejection {
                id: candidate.id.clone(),
                valid: true,
                reason: format!(
                    "costlier: {:.3} vs {:.3} {COST_UNIT}",
                    costs[&candidate.id].total, best_total
                ),
            });
        }
    }
    Ok(Selection {
        selected: selected.to_string(),
        costs,
        rejected,
    })
}

/// The first query whose output is not shown to meet its target, as a
/// reason. An output meets its target when its theoretical root guarantee
/// (composed in Stage 1 with the built-in model) satisfies it, or when the
/// deployment's empirical evidence shows that this candidate's output does
/// ([`crate::accuracy::AccuracyEvidenceProvider::output_guarantee`]). A
/// certified estimate inside the plan is not enough on its own: a difference
/// or ratio of certified estimates can be unbounded. Count-Min is admitted
/// only over update weights proven non-negative, which its bound assumes.
fn accuracy_violation(
    candidate: &PhysicalCandidate,
    targets: &[Option<AccuracyTarget>],
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
        let guarantee = root_guarantee(root);
        let meets = |g: &ResultGuarantee| models.accuracy.satisfies(g, target);
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

/// The theoretical guarantee of a query's output: the root node's own;
/// `scalar(v)` alone inherits `v`'s; any other scalar root is exact when
/// every operator it reads is, and has none otherwise (#669).
fn root_guarantee(root: &QueryRoot) -> Option<ResultGuarantee> {
    match root {
        QueryRoot::Operator(node) => node.guarantee.clone(),
        QueryRoot::Scalar(ScalarExpr::PromqlScalarFromVector(node)) => node.guarantee.clone(),
        QueryRoot::Scalar(expr) => expr
            .operator_refs()
            .into_iter()
            .all(|node| {
                node.guarantee
                    .as_ref()
                    .is_some_and(ResultGuarantee::is_exact)
            })
            .then(|| ResultGuarantee::exact("scalar over exact inputs")),
    }
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

/// Evaluations per second of a node computed once for `queries`
/// together. Repeating queries with equal intervals are evaluated together,
/// so each interval counts once. One-off, scheduled and unknown queries run
/// as one batch: the most invocations among them, amortized over
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
    let repeated: f64 = intervals
        .into_iter()
        .map(|ms| 1_000.0 / f64::from(ms))
        .sum();
    Ok(repeated + estimated + invocations as f64 / horizon_s)
}

/// The nodes each query reads first, in query order: query `i` is the scalar
/// root keyed `i` when there is one, else the next operator root.
fn query_starts(dag: &PhysicalASAPDAG) -> Vec<Vec<PhysicalASAPNodeId>> {
    let mut roots = dag.roots.iter();
    (0..dag.roots.len() + dag.scalar_roots.len())
        .map(|query| {
            match dag.scalar_roots.iter().find(|(position, _)| *position == query) {
                Some((_, expr)) => expr.operator_refs().into_iter().copied().collect(),
                None => roots.next().into_iter().copied().collect(),
            }
        })
        .collect()
}

/// How often each node of `dag` is computed: `(multiplier, per_second)`.
///
/// A materialized node is computed once for all its readers: per second at
/// ingestion time (`MatAtIngestion`: one second of arriving rows), or at the
/// evaluation rate of the queries reaching it together (`MatAtQuery`). A
/// `NotMaterialized` node is computed once per consumer that reaches it, at
/// that consumer's rate. A consumer is a query (operator or scalar root), at
/// its own evaluation rate, or a materialized node, at that node's rate; a
/// walk stops at the first materialized node it meets, and a node read
/// several times within one consumer's walk counts once for it. Its
/// `multiplier` is that number of consumers.
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

/// Price every node of `dag` per second of wall time. Nodes are exported
/// children first, so each node's input statistics are known when it is
/// reached. A query-time node is priced per computation over the rows its
/// time ranges keep, times how often it is computed per second
/// ([`computations`]). An ingestion-time node is priced over one second of
/// ingested rows; when a query-time node reads its state, that state is
/// also charged per retained byte per second (the window being built and
/// the completed one). Transient query-time memory is not priced.
fn price(
    dag: &PhysicalASAPDAG,
    recurrence: &[QueryRecurrence],
    data: &DataWorkload,
    stage3: &Stage3Calibration,
) -> Result<CandidateCost, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let first = dag.roots.first().copied().unwrap_or_default();
    stage3.validate().map_err(|error| (first, error))?;
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
        cost_per_cpu_op: stage3.cost_per_cpu_op,
        cost_per_scan_byte: stage3.cost_per_scan_byte,
        cost_per_retained_byte: 0.0,
        version: stage3.version.into(),
    };
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
    let multipliers =
        computations(dag, recurrence, stage3.horizon_s).map_err(|error| (first, error))?;
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
            // One second of ingested rows does not bound the groups a
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
                    let estimate = estimate_operator(
                        PhysicalOperator::HashAggregate {
                            grouping_key_count: keys,
                            accumulator_count: measures.len().max(1) as u64,
                        },
                        OperatorStatistics::HashAggregate {
                            edges: unary(out),
                            group_count,
                            key_bytes: 16 * keys,
                            accumulator_bytes_per_group: 8,
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
                let ops = input.rows as f64 * depth as f64;
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
        let (multiplier, per_second) = multipliers[&node.id];
        let mut cost = cost * per_second;
        let mut detail = format!("{detail}; x {per_second:.4}/s");
        let read_at_query_time = dag.edges.iter().any(|e| {
            e.producer == node.id && nodes[&e.consumer].output_state.timing == ExecutionTiming::QueryTime
        });
        if ingestion && read_at_query_time {
            let retained = 2 * out.rows * state_bytes(node);
            cost += stage3.cost_per_retained_byte_second * retained as f64;
            detail += &format!("; retains {retained} bytes");
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

/// Bytes per group of the state `node` keeps: a summary's state, or the row
/// of an exact state.
fn state_bytes(node: &asap_types::ir::physical_export::PhysicalASAPDAGNode) -> u64 {
    match &node.payload {
        Payload::ASAP(ASAPOp::SummaryAgg {
            family: family @ FieldDataType::Sketch(..),
            ..
        }) => summary_shape(family).1,
        _ => row_bytes(&node.output_schema),
    }
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

/// Update operations per input row and bytes per state.
fn summary_shape(family: &FieldDataType) -> (u64, u64) {
    match family {
        FieldDataType::Sketch(kind, _) => match kind.params() {
            SketchParams::Cms { width, depth } | SketchParams::CountSketch { width, depth } => {
                (u64::from(*depth), 8 * u64::from(*width) * u64::from(*depth))
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
                u64::from(*depth) + 1,
                8 * u64::from(*width) * u64::from(*depth) + 24 * u64::from(*heap_size),
            ),
            _ => (1, 1_024),
        },
        _ => (1, 8),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accuracy::AccuracyEvidenceProvider;
    use crate::stage2_physical_asap_optimization::optimize_physical_asap;
    use crate::test_support::lower_promql;
    use asap_types::ir::ExprSemantics;
    use asap_types::post_asap::{BoundExpr, ErrorMetric, ProbabilityExpr};
    use asap_types::pre_asap::expr_ir::ArithmeticOpKind;
    use asap_types::workload::{Evidence, Rate};

    /// `n` queries repeating every 10 s, as Example 1's panels do.
    fn every_10s(n: usize) -> Vec<QueryRecurrence> {
        vec![
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
                asap_types::workload::RepetitionInterval(10_000),
            ));
            n
        ]
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

    /// A query without a target constrains nothing: the Count-Min
    /// candidate is admitted and priced.
    #[test]
    fn count_min_is_admitted_without_a_target() {
        let selection =
            select_plan(
                &candidates(),
                &[None],
                &every_10s(1),
                &data(),
                PlanningModels::builtin(),
            ).unwrap();
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
        let cost = price(
            &candidate.dag,
            &every_10s(2),
            &data(),
            &Stage3Calibration::ILLUSTRATIVE,
        )
        .unwrap();
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
        let stored = price(
            &candidate.dag,
            &every_10s(2),
            &data(),
            &Stage3Calibration::ILLUSTRATIVE,
        )
        .unwrap();
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
        fn output_guarantee(&self, _output: &QueryRoot) -> Option<ResultGuarantee> {
            Some(ResultGuarantee::exact("measured"))
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

    /// `scalar(v)` alone inherits `v`'s guarantee; any other scalar
    /// expression over an approximate operand has none until #669; one over
    /// exact operands only is exact.
    #[test]
    fn scalar_of_one_vector_inherits_its_guarantee_and_nothing_else_does() {
        let node = lower_promql("sum(m)", AccuracyTarget::Exact);
        let approximate = ResultGuarantee {
            metric: ErrorMetric::AbsoluteValue,
            bound: BoundExpr::Constant { value: 0.01 },
            failure_probability: ProbabilityExpr::Constant { value: 0.001 },
            provenance: vec![],
        };
        let with = |guarantee: Option<ResultGuarantee>| {
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
        assert_eq!(root_guarantee(&doubled(with(Some(approximate)))), None);
        assert!(
            root_guarantee(&doubled(with(Some(ResultGuarantee::exact("exact")))))
                .is_some_and(|g| g.is_exact())
        );
    }

    /// A root repeating every `interval_ms`.
    fn repeating(interval_ms: u32) -> QueryRecurrence {
        QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(
            asap_types::workload::RepetitionInterval(interval_ms),
        ))
    }

    /// `quantile_over_time(0.99, m[1m])` with its KLL candidate chosen, its
    /// state `MatAtIngestion` when `ingestion`, else every node
    /// `NotMaterialized`.
    fn kll_dag(ingestion: bool) -> PhysicalASAPDAG {
        use asap_types::ir::{apply_materialization_timings, MaterializationAssignment, TimingMemo};
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
        let mut assignment = MaterializationAssignment::with_default(Materialization::NotMaterialized);
        if ingestion {
            assignment.set(
                &state,
                Materialization::MatAtIngestion(asap_types::ir::Medium::Memory),
            );
        }
        let timed = apply_materialization_timings(root, &assignment, &mut TimingMemo::new()).unwrap();
        asap_types::ir::physical_export::compile_physical_asap_workload_with_node_ids(&[
            QueryRoot::Operator(timed),
        ])
        .unwrap()
        .dag
    }

    fn node_of(
        dag: &PhysicalASAPDAG,
        pick: impl Fn(&Payload) -> bool,
    ) -> PhysicalASAPNodeId {
        dag.nodes.iter().find(|n| pick(&n.payload)).unwrap().id
    }

    fn is_scan(payload: &Payload) -> bool {
        matches!(payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
    }

    fn is_build(payload: &Payload) -> bool {
        matches!(payload, Payload::ASAP(ASAPOp::SummaryAgg { .. }))
    }

    fn priced(dag: &PhysicalASAPDAG, recurrence: &[QueryRecurrence]) -> CandidateCost {
        price(dag, recurrence, &data(), &Stage3Calibration::ILLUSTRATIVE).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1e-300)
    }

    /// An ingestion-time node is priced over one second of ingested rows,
    /// whatever its readers' evaluation rate; a query-time node scales with
    /// that rate.
    #[test]
    fn ingestion_time_node_is_priced_at_the_ingestion_rate() {
        let dag = kll_dag(true);
        let (fast, slow) = (priced(&dag, &[repeating(1_000)]), priced(&dag, &[repeating(10_000)]));
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

    /// The memory term is w × retained bytes: two windows of every group's
    /// state, for ingestion-time state that query time reads. Query-time
    /// state is transient and has none.
    #[test]
    fn memory_term_is_weight_times_retained_bytes() {
        let dag = kll_dag(true);
        let build = node_of(&dag, is_build);
        let Payload::ASAP(ASAPOp::SummaryAgg { family, .. }) =
            &dag.nodes.iter().find(|n| n.id == build).unwrap().payload
        else {
            unreachable!()
        };
        let cost = |dag: &PhysicalASAPDAG, w| {
            let calibration = Stage3Calibration {
                cost_per_retained_byte_second: w,
                ..Stage3Calibration::ILLUSTRATIVE
            };
            price(dag, &every_10s(1), &data(), &calibration).unwrap()
        };
        let (with, without) = (cost(&dag, 1.25e-7), cost(&dag, 0.0));
        // One state per series: data() declares 10 000.
        let retained = 2 * 10_000 * summary_shape(family).1;
        let term = with.per_node[&build].cost - without.per_node[&build].cost;
        assert!(close(term, 1.25e-7 * retained as f64), "{term}");
        assert!(close(with.total - without.total, term));
        let query_time = kll_dag(false);
        assert_eq!(cost(&query_time, 1.25e-7).total, cost(&query_time, 0.0).total);
    }

    /// A `NotMaterialized` node read by two queries is computed for each, at
    /// each one's rate; materialized, it is computed once per distinct
    /// interval of the queries reaching it.
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
        let single = priced(&one, &every_10s(1)).total;
        let mixed = [repeating(10_000), repeating(20_000)];
        assert!(close(priced(&two, &every_10s(2)).total, 2.0 * single));
        assert!(close(priced(&two, &mixed).total, 1.5 * single));
        let shared = two.roots[0];
        assert_eq!(shared, two.roots[1], "one root node for both queries");
        for node in &mut two.nodes {
            node.materialization = Materialization::MatAtQuery(asap_types::ir::Medium::Memory);
        }
        assert!(close(priced(&two, &every_10s(2)).total, single));
        assert!(close(priced(&two, &mixed).total, 1.5 * single));
    }

    /// One-off and unknown recurrence are amortized over the horizon H:
    /// invocations × per-evaluation cost / H.
    #[test]
    fn one_off_recurrence_is_amortized_over_the_horizon() {
        let root = lower_promql("sum by (job) (rate(m[1m]))", AccuracyTarget::Exact);
        let dag = optimize_physical_asap("L", &[QueryRoot::Operator(root)])
            .unwrap()
            .dag;
        let horizon = Stage3Calibration::ILLUSTRATIVE.horizon_s;
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
        let dag = kll_dag(false);
        for calibration in [
            Stage3Calibration {
                cost_per_retained_byte_second: -1.0,
                ..Stage3Calibration::ILLUSTRATIVE
            },
            Stage3Calibration {
                horizon_s: 0.0,
                ..Stage3Calibration::ILLUSTRATIVE
            },
        ] {
            assert!(price(&dag, &every_10s(1), &data(), &calibration).is_err());
        }
    }
}
