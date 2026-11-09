//! Costing a physical plan DAG node by node: each node's resource usage
//! ([`node_resource_usage_and_cost`]), then the plan's usage, latency and
//! cost ([`plan_cost`]). Moved from Stage 3 (`asap-stage3-plan-selection`,
//! whose module doc, "Cost model", describes the model), so Stage 3 computes
//! no cost of its own.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::node::analytical::AnalyticalCostError;
use crate::node::operator_statistics::EdgeStatistics;
use crate::node::state_bytes::{sketch_shape, StateInputRows};
use crate::node::usage::{cost_source, node_map, NodeMap};
use crate::plan::execution_demand::{computations, evaluation_rate, query_starts};
use crate::plan::read_back::read_back_ms;
use crate::{lookup, StateFamily};
use crate::{node_resource_usage_and_cost, NodeResourceUsageAndCost, ProducerOutput};
use crate::{
    CalibrationFit, CostInputs, CostModel, NodeCost, NodeCostSource, OperatorKind, ResourceUsage,
    MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp};
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
    PhysicalASAPOperatorPayload as Payload,
};
use asap_types::summary_type::SketchParams;
use asap_types::workload::{DataWorkload, QueryRecurrence, QueryWorkloadEntry, RangeEvaluation};

/// One candidate's usage and price over the whole workload: `usage` is
/// the whole candidate's, `per_node` one price per operator node of its
/// physical plan DAG.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanCost {
    pub usage: ResourceUsage,
    /// Its price per second under cost model 1 or 2 (under cost model 1, the
    /// sum of its nodes' charges plus request overhead for queries without
    /// operator nodes); `None` under cost model 3.
    pub cost_per_second: Option<f64>,
    /// Where its prices came from, from its nodes' [`NodeCost::cost_source`]:
    /// [`COST_SOURCE`](crate::COST_SOURCE) (all formulas), [`MEASURED_COST_SOURCE`] (all
    /// measured) or [`MIXED_COST_SOURCE`].
    pub source: &'static str,
    /// Each operator node of the candidate's physical plan DAG, by its id
    /// in that DAG, with its price.
    pub per_node: BTreeMap<PhysicalASAPNodeId, NodeCost>,
}

/// One query's latency per evaluation, its longest chain of work (module
/// doc, "Latency"): the compaction of the newest window of a state it reads
/// from ingestion time, then the query's own work, each on a full core.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueryLatency {
    /// Milliseconds to merge the ingest workers' partial copies of the
    /// newest window of the largest ingestion-time state the query reads;
    /// 0 when ingestion fits on one core or the query reads none.
    pub compaction_ms: f64,
    /// Milliseconds of the query-time work the query reaches, each node
    /// once.
    pub query_ms: f64,
}

impl QueryLatency {
    /// `compaction_ms + query_ms`: what is checked against the query's
    /// `max_latency_ms`.
    pub fn total_ms(&self) -> f64 {
        self.compaction_ms + self.query_ms
    }
}

/// Measure every node of `dag` per second of wall time (module doc, "Cost
/// model") and price it under `settings.cost_model`. Nodes are exported
/// children first, so each node's input statistics are known when it is
/// reached. A query-time node is measured per computation over the rows its
/// time ranges keep, times how often it is computed per second
/// ([`computations`]); an ingestion-time node over one second of arriving
/// rows. A materialized node's state is counted in byte-seconds per
/// second; a tumbling-window chain materialized beyond one evaluation keeps the tumbling windows its
/// `retention_ms` ($h_v$) covers. `entries[q]` says how often query `q`
/// runs and how many steps each run evaluates; the formulas are scaled by
/// `settings.frozen_calibration_factor` (1 when unset).
pub fn plan_cost(
    dag: &PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    settings: &CostInputs,
) -> Result<(PlanCost, CalibrationFit), (PhysicalASAPNodeId, AnalyticalCostError)> {
    let nodes = &node_map(dag);
    let recurrence: &[QueryRecurrence] = &entries
        .iter()
        .map(|entry| entry.recurrence.clone())
        .collect::<Vec<_>>();
    let range_evaluations: &[Option<RangeEvaluation>] = &entries
        .iter()
        .map(|entry| entry.range_evaluation)
        .collect::<Vec<_>>();
    let formula_scale = settings.frozen_calibration_factor.unwrap_or(1.0);
    let first = dag.nodes.first().map(|n| n.id).unwrap_or_default();
    let table = &settings.measured_operation_costs;
    let mut fit = CalibrationFit::default();
    let speed = settings.resource_speed;
    // Cost model 1 prices each node as it is measured.
    let usage_prices = match &settings.cost_model {
        CostModel::UsageOverTime(prices) => Some(prices),
        _ => None,
    };
    let EachNodeResourceUsageAndCost {
        priced,
        multipliers,
    } = each_node_resource_usage_and_cost(
        dag,
        nodes,
        recurrence,
        range_evaluations,
        data,
        settings,
        formula_scale,
        None,
    )?;
    let mut output: HashMap<PhysicalASAPNodeId, EdgeStatistics> = HashMap::new();
    let mut per_node = BTreeMap::new();
    // The candidate's usage, accumulated node by node; peak cores and
    // latency are filled in once every node is measured.
    let mut usage = ResourceUsage::default();
    // Each node's work in ms on one core: one computation at query time,
    // one second of arriving rows at ingestion time.
    let mut work_ms: HashMap<PhysicalASAPNodeId, f64> = HashMap::new();
    // The raw data query-time scans read, retained on disk: per source, the
    // largest scan's (bytes, node).
    let mut raw_data: HashMap<&Source, (u64, PhysicalASAPNodeId)> = HashMap::new();
    // Each in-memory ingestion-time state's bytes per window (what it keeps
    // for one window, its output or an exact aggregate's values).
    let mut maintained: Vec<(PhysicalASAPNodeId, u64)> = Vec::new();
    // Each summary node's state family, to look up in the measured table:
    // a build's own (one sketch instance per group; a shared Hydra
    // structure is not that sketch), inherited by the merges, estimates and
    // finalizations over it.
    let mut family_of: HashMap<PhysicalASAPNodeId, StateFamily<'_>> = HashMap::new();
    let sketch_size =
        |params: &SketchParams| sketch_shape(params, StateInputRows::UNKNOWN).1 as f64;
    for node in &dag.nodes {
        let node_priced = &priced[&node.id];
        usage.cpu_ops_per_second += node_priced.cpu_ops_per_second;
        usage.scanned_bytes_per_second += node_priced.scanned_bytes_per_second;
        usage.average_memory_bytes += node_priced.average_memory_bytes;
        usage.peak_memory_bytes = usage
            .peak_memory_bytes
            .saturating_add(node_priced.peak_memory_bytes);
        usage.disk_bytes = usage.disk_bytes.saturating_add(node_priced.disk_bytes);
        fit.add(node_priced.fit);
        work_ms.insert(node.id, node_priced.work_ms_per_execution);
        output.insert(node.id, node_priced.output);
        if node_priced.family != StateFamily::None {
            family_of.insert(node.id, node_priced.family);
        }
        if let Some(window) = node_priced.maintained {
            maintained.push((node.id, window));
        }
        if let Some((source, bytes)) = node_priced.raw_scan {
            let largest = raw_data.entry(source).or_insert((bytes, node.id));
            if bytes > largest.0 {
                *largest = (bytes, node.id);
            }
        }
        per_node.insert(node.id, node_priced.cost.clone());
    }
    // The runtime's fixed cost per query request, charged to the node the
    // query reads first and added to its latency below.
    let evaluation_ms = table.ns_per_evaluation * 1e-6;
    let starts = query_starts(dag);
    let mut evaluation_ms_per_query = vec![0.0; starts.len()];
    let mut request_ms_per_second = 0.0;
    // Literal-only roots have no operator to own their fee. Keep that charge
    // at plan level while preserving existing attribution for operator queries.
    let mut unattached_request_cost = 0.0;
    let mut has_unattached_request_overhead = false;
    for (query, start) in starts.iter().enumerate() {
        if evaluation_ms <= 0.0 {
            continue;
        }
        let diagnostic_node = start.first().copied().unwrap_or(first);
        let requests_per_second = evaluation_rate(
            recurrence.get(query).map(|r| (r, None)),
            settings.non_repeating_query_horizon_seconds,
        )
        .map_err(|error| (diagnostic_node, error))?;
        let ops_per_second = evaluation_ms / speed.ms_per_cpu_op * requests_per_second;
        usage.cpu_ops_per_second += ops_per_second;
        request_ms_per_second += evaluation_ms * requests_per_second;
        if !usage.cpu_ops_per_second.is_finite() || !request_ms_per_second.is_finite() {
            return Err((diagnostic_node, AnalyticalCostError::Overflow));
        }
        evaluation_ms_per_query[query] = evaluation_ms;
        let Some(&first_read) = start.first() else {
            has_unattached_request_overhead = true;
            if let Some(prices) = usage_prices {
                unattached_request_cost += prices.cost_per_cpu_op * ops_per_second;
                if !unattached_request_cost.is_finite() {
                    return Err((diagnostic_node, AnalyticalCostError::Overflow));
                }
            }
            continue;
        };
        let node = per_node
            .get_mut(&first_read)
            .expect("every node is measured");
        node.cpu_core_seconds_per_second += evaluation_ms * requests_per_second / 1_000.0;
        if !node.cpu_core_seconds_per_second.is_finite() {
            return Err((first_read, AnalyticalCostError::Overflow));
        }
        node.detail += &format!(
            "; q{} request overhead {:.3} ns x {requests_per_second:.4}/s",
            query + 1,
            table.ns_per_evaluation
        );
        if let (Some(cost), Some(prices)) = (&mut node.cost_per_second, usage_prices) {
            *cost += prices.cost_per_cpu_op * ops_per_second;
            if !cost.is_finite() {
                return Err((first_read, AnalyticalCostError::Overflow));
            }
        }
    }
    for (bytes, id) in raw_data.into_values() {
        let node = per_node.get_mut(&id).expect("every node is measured");
        usage.disk_bytes = usage.disk_bytes.saturating_add(bytes);
        node.detail += &format!("; keeps {bytes} bytes of raw data on disk");
        if let (Some(cost), Some(prices)) = (&mut node.cost_per_second, usage_prices) {
            *cost += prices.cost_per_disk_byte_second * bytes as f64;
        }
    }
    // With k ingest workers, each of the k − 1 beyond the first builds its
    // own partial copy of an in-memory ingestion-time state's newest window
    // (merged at query time, [`query_latencies`]), held at the peak too.
    for (id, window) in maintained {
        let copies = ingest_workers(dag, &work_ms, id) - 1;
        if copies == 0 {
            continue;
        }
        let bytes = copies.saturating_mul(window);
        let measured = per_node.get_mut(&id).expect("every node is measured");
        usage.peak_memory_bytes = usage.peak_memory_bytes.saturating_add(bytes);
        measured.detail +=
            &format!("; {copies} partial copies of its newest window, {bytes} bytes");
    }
    // Milliseconds of one merge of each state with a measured query-time
    // merge entry (measured ns · 10⁻⁶), for the newest window's compaction.
    let merge_ms: HashMap<PhysicalASAPNodeId, f64> = family_of
        .iter()
        .filter_map(|(id, family)| {
            lookup(
                table,
                OperatorKind::SummaryMerge,
                ExecutionTiming::QueryTime,
                *family,
                sketch_size,
            )
            .map(|unit| (*id, (unit.ns_per_unit + unit.ns_per_invocation) * 1e-6))
        })
        .collect();
    let mut latency = query_latencies(
        dag,
        nodes,
        &output,
        &work_ms,
        &merge_ms,
        speed.ms_per_cpu_op * formula_scale,
    );
    for (latency, evaluation_ms) in latency.iter_mut().zip(evaluation_ms_per_query) {
        latency.query_ms += evaluation_ms;
    }
    usage.peak_cpu_cores = peak_cpu_cores(
        dag,
        nodes,
        &work_ms,
        &multipliers,
        &latency,
        request_ms_per_second,
    );
    usage.latency_per_query = latency;
    let cost = match &settings.cost_model {
        CostModel::UsageOverTime(_) => {
            let node_cost: Option<f64> = per_node.values().map(|node| node.cost_per_second).sum();
            let total = node_cost.map(|cost| cost + unattached_request_cost);
            if total.is_some_and(|cost| !cost.is_finite()) {
                return Err((first, AnalyticalCostError::Overflow));
            }
            total
        }
        CostModel::ProvisionedCapacity(prices) => {
            let cost = prices.cost_per_provisioned_memory_byte_second
                * usage.peak_memory_bytes as f64
                + prices.cost_per_provisioned_cpu_core_second * f64::from(usage.peak_cpu_cores)
                + prices.cost_per_disk_byte_second * usage.disk_bytes as f64;
            if !cost.is_finite() {
                return Err((first, AnalyticalCostError::Overflow));
            }
            Some(cost)
        }
        CostModel::ClusterCapacity(_) => None,
    };
    Ok((
        PlanCost {
            usage,
            cost_per_second: cost,
            source: if has_unattached_request_overhead {
                if per_node
                    .values()
                    .all(|node| node.cost_source == NodeCostSource::Measured)
                {
                    MEASURED_COST_SOURCE
                } else {
                    MIXED_COST_SOURCE
                }
            } else {
                cost_source(&per_node)
            },
            per_node,
        },
        fit,
    ))
}

/// Every node of a physical DAG priced ([`each_node_resource_usage_and_cost`]), with each
/// node's `(multiplier, per_second)` ([`computations`]).
struct EachNodeResourceUsageAndCost<'a> {
    priced: HashMap<PhysicalASAPNodeId, NodeResourceUsageAndCost<'a>>,
    multipliers: HashMap<PhysicalASAPNodeId, (u64, f64)>,
}

/// [`node_resource_usage_and_cost`] over every node of `dag`, children first, with the
/// arguments of [`plan_cost`]. `root_rates`, when given, are
/// the computations per second of each root's consumer in place of its
/// query's own rate ([`computations`]).
#[allow(clippy::too_many_arguments)]
fn each_node_resource_usage_and_cost<'a>(
    dag: &'a PhysicalASAPDAG,
    nodes: &NodeMap<'a>,
    recurrence: &[QueryRecurrence],
    range_evaluations: &[Option<RangeEvaluation>],
    data: &DataWorkload,
    settings: &'a CostInputs,
    formula_scale: f64,
    root_rates: Option<&[f64]>,
) -> Result<EachNodeResourceUsageAndCost<'a>, (PhysicalASAPNodeId, AnalyticalCostError)> {
    let first = dag.nodes.first().map(|n| n.id).unwrap_or_default();
    let multipliers = computations(
        dag,
        recurrence,
        range_evaluations,
        settings.non_repeating_query_horizon_seconds,
        root_rates,
    )
    .map_err(|error| (first, error))?;
    let mut producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    let mut consumers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for e in &dag.edges {
        producers_of.entry(e.consumer).or_default().push(e.producer);
        consumers_of.entry(e.producer).or_default().push(e.consumer);
    }
    let mut priced: HashMap<PhysicalASAPNodeId, NodeResourceUsageAndCost<'a>> = HashMap::new();
    for node in &dag.nodes {
        let views: Vec<ProducerOutput<'a>> = producers_of
            .get(&node.id)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|p| {
                let produced: &NodeResourceUsageAndCost<'a> = &priced[p];
                ProducerOutput {
                    payload: &nodes[p].payload,
                    output_schema: &nodes[p].output_schema,
                    output: produced.output,
                    sources: produced.sources.clone(),
                    family: produced.family,
                    unchanged_columns: produced.unchanged_columns.clone(),
                    tumbling_windows: produced.tumbling_windows,
                }
            })
            .collect();
        let window_ms = match node.output_state.timing {
            ExecutionTiming::IngestionTime => maintained_window_ms(node.id, nodes, &producers_of),
            ExecutionTiming::QueryTime => scan_read_ms(node, nodes, &consumers_of),
        };
        let node_priced = node_resource_usage_and_cost(
            node,
            &views,
            multipliers[&node.id],
            window_ms,
            data,
            settings,
            formula_scale,
        )
        .map_err(|error| (node.id, error))?;
        priced.insert(node.id, node_priced);
    }
    Ok(EachNodeResourceUsageAndCost {
        priced,
        multipliers,
    })
}

/// The window a query-time `Scan` `node` reads per execution: the most
/// offset + range its consumers read ([`read_back_ms`], no range-evaluation
/// span), once however many consumers read it. `None` for any other node,
/// or when a consumer reads it whole.
fn scan_read_ms(
    node: &PhysicalASAPDAGNode,
    nodes: &NodeMap<'_>,
    consumers_of: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
) -> Option<u64> {
    if !matches!(node.payload, Payload::NonASAP(NonASAPOp::Scan { .. })) {
        return None;
    }
    let readers = |id: PhysicalASAPNodeId| consumers_of.get(&id).cloned().unwrap_or_default();
    read_back_ms(
        readers(node.id),
        |id| &nodes[&id].payload,
        readers,
        |_| 0,
        true,
    )
}

/// The CPU cores a candidate needs at its peak: enough for its
/// ingestion-time work every second (none without any), plus one per
/// query with query-time work, all evaluating at once; at least the cores
/// its average query-time work fills.
fn peak_cpu_cores(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode>,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    multipliers: &HashMap<PhysicalASAPNodeId, (u64, f64)>,
    latency: &[QueryLatency],
    request_ms_per_second: f64,
) -> u32 {
    let mut ingestion_ms_per_second = 0.0;
    // Request overhead is attributed in per_node reports, not work_ms, and so
    // must enter the throughput floor separately (including literal queries).
    let mut query_ms_per_second = request_ms_per_second;
    for node in &dag.nodes {
        let per_second = multipliers[&node.id].1;
        if nodes[&node.id].output_state.timing == ExecutionTiming::IngestionTime {
            ingestion_ms_per_second += work_ms[&node.id] * per_second;
        } else {
            query_ms_per_second += work_ms[&node.id] * per_second;
        }
    }
    let cores = |ms_per_second: f64| (ms_per_second / 1_000.0).ceil() as u32;
    let evaluating = latency.iter().filter(|l| l.query_ms > 0.0).count() as u32;
    cores(ingestion_ms_per_second) + evaluating.max(cores(query_ms_per_second))
}

/// Each query's latency per evaluation (module doc, "Latency"): the sum of
/// the query-time work it reaches, each node once, plus the longest
/// compaction among the ingestion-time states it reads. Compacting a
/// state's newest window merges, for each of its `I` states per window,
/// the `k` ingest workers' partial copies, `(k − 1)·I` merges, where `k`
/// is the cores its ingestion work takes, at least one. Each merge takes
/// the state's measured merge time (`merge_ms`) when the measured table has
/// one, and one operation otherwise.
fn query_latencies(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode>,
    output: &HashMap<PhysicalASAPNodeId, EdgeStatistics>,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    merge_ms: &HashMap<PhysicalASAPNodeId, f64>,
    formula_merge_ms: f64,
) -> Vec<QueryLatency> {
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    let at_ingestion =
        |id: PhysicalASAPNodeId| nodes[&id].output_state.timing == ExecutionTiming::IngestionTime;
    let compaction_ms = |state: PhysicalASAPNodeId| {
        let workers = ingest_workers(dag, work_ms, state) as f64;
        let states = output[&state].rows as f64;
        let per_merge_ms = merge_ms.get(&state).copied().unwrap_or(formula_merge_ms);
        (workers - 1.0) * states * per_merge_ms
    };
    query_starts(dag)
        .into_iter()
        .map(|start| {
            let mut latency = QueryLatency {
                compaction_ms: 0.0,
                query_ms: 0.0,
            };
            let mut seen = HashSet::new();
            let mut stack = start;
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if at_ingestion(id) {
                    latency.compaction_ms = latency.compaction_ms.max(compaction_ms(id));
                    continue;
                }
                latency.query_ms += work_ms[&id];
                stack.extend(inputs(id));
            }
            latency
        })
        .collect()
}

/// The cores `state`'s ingestion work takes, at least one: the ingest
/// workers that each build a partial copy of its newest window.
fn ingest_workers(
    dag: &PhysicalASAPDAG,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    state: PhysicalASAPNodeId,
) -> u64 {
    let mut ingest_ms_per_s = 0.0;
    let mut seen = HashSet::new();
    let mut stack = vec![state];
    while let Some(id) = stack.pop() {
        if seen.insert(id) {
            ingest_ms_per_s += work_ms[&id];
            stack.extend(
                dag.edges
                    .iter()
                    .filter(|e| e.consumer == id)
                    .map(|e| e.producer),
            );
        }
    }
    (ingest_ms_per_s / 1_000.0).ceil().max(1.0) as u64
}

/// The window an ingestion-time grouping `node` maintains: a tumbling-window chain's
/// tumbling windows, or the time range below it (through filters, projections and
/// time shifts). `None` when there is none.
fn maintained_window_ms(
    node: PhysicalASAPNodeId,
    nodes: &NodeMap<'_>,
    producers_of: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
) -> Option<u64> {
    match &nodes[&node].payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => return Some(*tumbling_window_width_ms),
        Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => {
            return Some(range.as_millis() as u64)
        }
        _ => {}
    }
    let producer = *producers_of.get(&node)?.first()?;
    match &nodes[&producer].payload {
        Payload::NonASAP(
            NonASAPOp::TimeRange { .. }
            | NonASAPOp::Filter { .. }
            | NonASAPOp::Project { .. }
            | NonASAPOp::TimeShift { .. },
        ) => maintained_window_ms(producer, nodes, producers_of),
        _ => None,
    }
}
