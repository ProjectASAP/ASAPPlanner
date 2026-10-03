//! #509 Stage 3 (MVP): plan selection, the only stage that computes cost.
//!
//! Each Stage 2 candidate is checked against every query's accuracy target
//! with the accuracy model, and Count-Min is admitted only over weights proven
//! non-negative; a miss rejects the candidate as invalid, with a reason.
//! Every valid candidate is priced node by node over its physical DAG, so a
//! node shared by several queries is charged once, and the cheapest is
//! selected. The rest are reported valid but costlier.
//!
//! Prices come from [`crate::analytical_cost::estimate_operator`] over edge
//! statistics derived from the [`DataWorkload`] and a fixed default group
//! count; summary build and estimation are priced as rows × sketch depth and
//! groups × k. These numbers are illustrative, not calibrated. Latency
//! bounds and deployment capabilities are not checked yet.
use asap_types::ir::NonASAPOp;
use std::collections::{BTreeMap, HashMap};

use asap_types::ir::operator::Reduction;
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::schema::{DataType, Schema};
use asap_types::ir::schema::{
    FieldDataType, SketchAlgorithm, SketchParams, SketchStatistic, WeightDomain,
};
use asap_types::ir::{ASAPOp, Operator, OperatorNode};
use asap_types::types::AccuracyTarget;
use asap_types::workload::DataWorkload;
use thiserror::Error;

use crate::analytical_cost::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceCalibration, ResourceEstimate,
};
use crate::physical_candidates::PhysicalCandidate;
use crate::physical_operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::PlanningModels;

pub const COST_UNIT: &str = "cpu_ms_per_workload_evaluation";
pub const COST_SOURCE: &str = "analytical-cost-v1 (illustrative statistics)";

/// Groups assumed for every `by (...)` reduction, absent group-count evidence.
const DEFAULT_GROUP_COUNT: u64 = 100;
/// Used only when the data workload does not declare them.
const DEFAULT_SERIES: u64 = 1_000;
const DEFAULT_ROWS_PER_SECOND: f64 = 1_000.0;
const DEFAULT_LOOKBACK_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct NodeCost {
    pub cost: f64,
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

/// Reject candidates that miss a query's accuracy target, price the rest and
/// select the cheapest (the first on ties). `targets[i]` is the requirement
/// of `candidate.roots[i]`; `None` imposes none.
pub fn stage3_select(
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
        if let Some(reason) = accuracy_violation(candidate, targets, &models) {
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

/// The first summary estimate that misses its query's target, as a reason.
fn accuracy_violation(
    candidate: &PhysicalCandidate,
    targets: &[Option<AccuracyTarget>],
    models: &PlanningModels<'_>,
) -> Option<String> {
    for (query, (root, target)) in candidate.roots.iter().zip(targets).enumerate() {
        let Some(target) = target else { continue };
        for node in OperatorNode::reachable(root) {
            let Operator::ASAP(ASAPOp::SummaryEstimate {
                summary_input,
                query: statistic,
            }) = &node.operator
            else {
                continue;
            };
            let Operator::ASAP(ASAPOp::SummaryAgg { family, input, .. }) = &summary_input.operator
            else {
                return Some(format!("q{}: estimate over a non-summary input", query + 1));
            };
            let name = family_name(family);
            // Count-Min's one-sided error bound assumes no negative updates.
            if matches!(family, FieldDataType::Sketch(kind, _)
                    if matches!(kind.algorithm(), SketchAlgorithm::Cms | SketchAlgorithm::CmsWithHeap))
                && !matches!(input.weight_domain, WeightDomain::NonNegative { .. })
            {
                return Some(format!(
                    "q{}: {name} needs non-negative update weights, and these are not proven \
                     non-negative",
                    query + 1
                ));
            }
            let Some(guarantee) = models.accuracy.local_guarantee(family, statistic) else {
                return Some(format!(
                    "q{}: no accuracy model for {name}; target {target:?}",
                    query + 1
                ));
            };
            if !models.accuracy.satisfies(&guarantee, target) {
                return Some(format!(
                    "q{}: {name} guarantees bound {:?}, failure probability {:?}, which misses \
                     target {target:?} (analytical guarantee; no accuracy evidence)",
                    query + 1,
                    guarantee.bound.evaluate(),
                    guarantee.failure_probability.evaluate(),
                ));
            }
        }
    }
    None
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

/// Price every node of `dag` once. Nodes are exported children first, so
/// each node's input statistics are known when it is reached.
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
        cost_per_retained_byte: 0.0,
        version: "illustrative-v1".into(),
    };
    let nodes: HashMap<_, _> = dag.nodes.iter().map(|n| (n.id, n)).collect();
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
        output.insert(node.id, out);
        per_node.insert(node.id, NodeCost { cost, detail });
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
    use crate::physical_candidates::stage2_physical;
    use crate::test_support::lower_promql;
    use asap_types::ir::QueryRoot;
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
        let inventory = crate::logical_candidates::enumerate_local_logical_candidates(vec![(
            0,
            QueryRoot::Operator(root),
        )])
        .unwrap();
        let topk = inventory
            .targets
            .iter()
            .position(|t| t.alternatives.len() > 2)
            .unwrap();
        [0, 1, 2]
            .into_iter()
            .map(|alternative| {
                let mut choice = vec![0; inventory.targets.len()];
                choice[topk] = alternative;
                let roots: Vec<_> =
                    crate::logical_candidates::compose_logical_candidate(&inventory, &choice)
                        .unwrap()
                        .into_iter()
                        .map(|(_, root)| match root {
                            QueryRoot::Operator(node) => node,
                            QueryRoot::Scalar(_) => panic!("operator root"),
                        })
                        .collect();
                let mut candidate = stage2_physical("L", &roots).unwrap();
                candidate.id = format!("P{}", alternative + 1);
                candidate
            })
            .collect()
    }

    /// A summary whose analytical guarantee misses the target is rejected as
    /// invalid with a reason; the exact plan is then selected.
    #[test]
    fn accuracy_failing_candidate_is_rejected_as_invalid() {
        let candidates = candidates();
        let strict = AccuracyTarget::EpsilonDelta {
            epsilon: 1e-6,
            delta: 1e-9,
        };
        let selection = stage3_select(
            &candidates,
            &[Some(strict)],
            &data(),
            PlanningModels::builtin(),
        )
        .unwrap();
        assert_eq!(selection.selected, "P1");
        let rejected: BTreeMap<_, _> = selection
            .rejected
            .iter()
            .map(|r| (r.id.as_str(), r))
            .collect();
        assert_eq!(rejected.len(), 2);
        assert!(!rejected["P3"].valid);
        assert!(
            rejected["P3"].reason.contains("misses target"),
            "{}",
            rejected["P3"].reason
        );
        assert!(!selection.costs.contains_key("P3"));
    }

    /// Count-Min over weights not proven non-negative is invalid whatever
    /// the target.
    #[test]
    fn count_min_over_signed_weights_is_rejected_as_invalid() {
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = stage3_select(
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

    /// Each node is priced once, under its DAG id, and the total is the sum;
    /// every candidate is either selected or rejected as costlier.
    #[test]
    fn per_node_costs_cover_the_dag_and_sum_to_the_total() {
        let candidates = candidates();
        let target = AccuracyTarget::EpsilonDelta {
            epsilon: 0.01,
            delta: 0.001,
        };
        let selection = stage3_select(
            &candidates,
            &[Some(target)],
            &data(),
            PlanningModels::builtin(),
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
}
