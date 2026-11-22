//! Costing a physical plan DAG node by node: each node's resource usage
//! ([`node_resource_usage_and_cost`]), then the plan's usage, latency and
//! cost ([`plan_cost`]). Stage 3 costs its candidates with the same
//! function and rules (`executions_per_second`, `RefreshWindow::computed_from`,
//! `read_back_ms`, [`raw_data_disk_cost_per_second`],
//! [`request_overhead_cost_per_second`]), so its objective is the
//! `plan_cost` of the plan it selects.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::node::analytical::AnalyticalCostError;
use crate::node::operations_per_unit::merge_terms_per_state;
use crate::node::operator_statistics::EdgeStatistics;
use crate::node::state_bytes::{sketch_state_bytes, state_bytes_per_row, StateInputRows};
use crate::node::usage::{cost_source, reads_one_batch_of};
use crate::plan::execution_demand::{computations, evaluation_rate, query_starts};
use crate::plan::resident_windows::{resident_windows, ChainReader, ReadTimes, WindowRead};
use crate::{lookup_at_scale, StateFamily};
use crate::{node_resource_usage_and_cost, NodeResourceUsageAndCost, ProducerOutput};
use crate::{
    CalibrationFit, CostInputs, CostModel, NodeCost, NodeCostSource, OperatorKind,
    ResourceUnitCost, ResourceUsage, ViewWindowCache, MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
use asap_types::ir::operator::Source;
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::NonASAPOp;
use asap_types::plan::materialized_view::RefreshWindow;
use asap_types::plan::physical_asap_dag::{
    PhysicalASAPDAG, PhysicalASAPDAGNode, PhysicalASAPNodeId,
    PhysicalASAPOperatorPayload as Payload,
};
use asap_types::plan::time_window::{read_back_ms, windows_readers_need};
use asap_types::stages::Lifetime;
use asap_types::summary_type::SketchParams;
use asap_types::workload::{
    greatest_common_divisor, AssumedDataFact, DataWorkload, QueryWorkloadEntry,
};

/// One candidate's usage and cost over the whole workload: `usage` is
/// the whole candidate's, `per_node` one cost per operator node of its
/// physical plan DAG.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanCost {
    pub usage: ResourceUsage,
    /// Its cost per second under cost model 1 or 2 (under cost model 1, the
    /// sum of its nodes' charges plus request overhead for queries without
    /// operator nodes); `None` under cost model 3.
    pub cost_per_second: Option<f64>,
    /// Where its costs came from, from its nodes' [`NodeCost::cost_source`]:
    /// [`COST_SOURCE`](crate::COST_SOURCE) (all formulas), [`MEASURED_COST_SOURCE`] (all
    /// measured) or [`MIXED_COST_SOURCE`].
    pub source: &'static str,
    /// Each operator node of the candidate's physical plan DAG, by its id
    /// in that DAG, with its cost.
    pub per_node: BTreeMap<PhysicalASAPNodeId, NodeCost>,
}

impl PlanCost {
    /// The data facts the data workload does not declare that its nodes'
    /// costs assumed ([`NodeCost::assumed_data_facts`]), each once, in node
    /// order.
    pub fn assumed_data_facts(&self) -> Vec<AssumedDataFact> {
        let mut facts = Vec::new();
        for fact in self
            .per_node
            .values()
            .flat_map(|node| &node.assumed_data_facts)
        {
            // A plan names each assumption once, however many of its nodes
            // rest on it.
            if !facts.contains(fact) {
                facts.push(fact.clone());
            }
        }
        facts
    }
}

/// One query's latency per request: the freshness wait (the allowed
/// lateness, then the refresh of the newest window of every ingestion-time
/// view it reads, queued behind the refreshes closing at the same instant),
/// then the request's execution on one query thread
/// (`docs/design_docs/proposals/stage3-lowest-mean-latency.md`, section
/// 2.1).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct QueryLatency {
    /// The data plane's allowed lateness
    /// ([`DataPlaneExecution::allowed_lateness_ms`](crate::DataPlaneExecution)).
    pub allowed_lateness_ms: f64,
    /// Queueing of the refresh group it waits longest for: behind the other
    /// groups closing at the same instant (divided among the precompute
    /// threads) and the groups it reads; 0 when it reads no view.
    pub precompute_queueing_ms: f64,
    /// The wall time of that group's refresh (its nodes' refresh CPU, the
    /// merging of the ingest threads' partial copies and the I/O wait); 0
    /// when it reads no view.
    pub precompute_execution_ms: f64,
    /// The request's query-time work, each node it reaches once, plus the
    /// fixed cost per request and per step.
    pub query_execution_ms: f64,
    /// Its evaluations per second (a range request's steps each count),
    /// its weight in the mean latency.
    pub evaluations_per_second: f64,
}

impl QueryLatency {
    /// The latency per request: the four parts' sum.
    pub fn latency_ms(&self) -> f64 {
        self.allowed_lateness_ms
            + self.precompute_queueing_ms
            + self.precompute_execution_ms
            + self.query_execution_ms
    }
}

/// The CPU a plan's work of one thread role uses, and the threads that
/// serve it at the target utilisation
/// ([`DataPlaneExecution::max_thread_utilization`](crate::DataPlaneExecution)).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleThreads {
    /// Thread-seconds per second the role keeps its threads busy: its CPU
    /// core-seconds, and for the precompute role a refresh's I/O wait too
    /// (a waiting thread is busy).
    pub predicted_busy_thread_seconds_per_second: f64,
    /// ceil(busy thread-seconds per second / max_thread_utilization), at
    /// least 1.
    pub recommended_threads: u32,
}

impl RoleThreads {
    fn of(cores: f64, utilization: f64) -> Self {
        Self {
            predicted_busy_thread_seconds_per_second: cores,
            recommended_threads: ((cores / utilization).ceil() as u32).max(1),
        }
    }
}

/// The threads a deployment needs to serve a plan, per role.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecommendedThreads {
    /// Inserting arriving rows, the decode and append outside the plan
    /// included (`MeasuredOperationCosts::ns_per_ingested_sample`).
    pub ingest: RoleThreads,
    /// Window-close refreshes.
    pub precompute: RoleThreads,
    /// Query requests.
    pub query: RoleThreads,
}

/// Measure every node of `dag` per second of wall time (module doc, "Cost
/// model") and cost it under `settings.cost_model`. Nodes are exported
/// children first, so each node's input statistics are known when it is
/// reached. A query-time node is measured per computation over the rows its
/// time ranges keep, times how often it is computed per second
/// ([`computations`]); an ingestion-time node over one second of arriving
/// rows. A materialized node's state is counted in byte-seconds per
/// second; a node materialized across evaluations keeps what its
/// `retention_ms` ($h_v$) covers ([`node_resource_usage_and_cost`]).
/// `entries[q]` says how often query `q` runs and how many steps each run
/// evaluates; the formulas are scaled by `settings.frozen_calibration_factor`
/// (1 when unset).
pub fn plan_cost(
    dag: &PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    settings: &CostInputs,
) -> Result<(PlanCost, CalibrationFit), (PhysicalASAPNodeId, AnalyticalCostError)> {
    let nodes = &node_map(dag);
    let first = dag.nodes.first().map(|n| n.id).unwrap_or_default();
    let table = &settings.measured_operation_costs;
    let mut fit = CalibrationFit::default();
    // Cost model 1 costs each node as it is measured.
    let unit_costs = settings.cost_model.per_node_unit_costs();
    // Each node's `(multiplier, per_second)` ([`computations`]), then its
    // usage and cost ([`node_resource_usage_and_cost`]), children first.
    let multipliers = computations(dag, entries, settings.non_repeating_query_horizon_seconds)
        .map_err(|error| (first, error))?;
    // Each node's producers in its payload's input order, whatever order
    // the DAG lists its edges in (`PhysicalASAPDAGNode::producers`): its
    // first producer is its child.
    let producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = dag
        .nodes
        .iter()
        .map(|node| (node.id, node.producers()))
        .collect();
    let first_input_of = |id: PhysicalASAPNodeId| {
        let input = producers_of[&id].first()?;
        Some(&nodes[input].payload)
    };
    // Each node keeping raw rows at ingestion, not stored: what an
    // ingestion-time reader of it reads, one batch
    // ([`reads_one_batch_of`]).
    let one_batch_nodes: HashMap<PhysicalASAPNodeId, PhysicalASAPDAGNode> = dag
        .nodes
        .iter()
        .filter(|node| node.keeps_raw_rows_at_ingestion(first_input_of(node.id)))
        .map(|node| {
            let mut one_batch = node.clone();
            one_batch.lifetime = Lifetime::None;
            one_batch.retention_ms = None;
            (node.id, one_batch)
        })
        .collect();
    // Each node's readers' evaluation grid: the gcd of the grids of the
    // queries whose roots reach it, each its evaluation context's grid
    // (`QueryWorkloadEntry::evaluation_grid`, the grid Stage 3's context
    // of the query has; a cover is charged its windows over it).
    let mut grid_of: HashMap<PhysicalASAPNodeId, u64> = HashMap::new();
    for (query, &root) in dag.roots.iter().enumerate() {
        let Some(entry) = entries.get(query) else {
            continue;
        };
        let period_ms = entry.evaluation_grid().period_ms;
        let mut seen = std::collections::HashSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                let known = grid_of.entry(id).or_insert(0);
                *known = asap_types::workload::greatest_common_divisor(*known, period_ms);
                pending.extend(producers_of.get(&id).into_iter().flatten().copied());
            }
        }
    }
    let evaluation_grid_of = |id: PhysicalASAPNodeId| {
        grid_of
            .get(&id)
            .filter(|&&period_ms| period_ms > 0)
            .map(|&period_ms| asap_types::stages::EvaluationGrid {
                period_ms,
                phase_ms: 0,
            })
    };
    let mut costed: HashMap<PhysicalASAPNodeId, NodeResourceUsageAndCost<'_>> = HashMap::new();
    let mut one_batch: HashMap<PhysicalASAPNodeId, NodeResourceUsageAndCost<'_>> = HashMap::new();
    for node in &dag.nodes {
        let views: Vec<ProducerOutput<'_>> = producers_of[&node.id]
            .iter()
            .map(|p| {
                let read = match reads_one_batch_of(node, nodes[p], first_input_of(*p)) {
                    true => &one_batch[p],
                    false => &costed[p],
                };
                read.producer_output(&nodes[p].payload, &nodes[p].output_schema)
            })
            .collect();
        let window_ms = match node.output_state.timing {
            ExecutionTiming::IngestionTime => {
                dag.refresh_window(node.id).map(|window| window.width_ms)
            }
            ExecutionTiming::QueryTime => scan_read_ms(dag, node),
        };
        let node_costed = node_resource_usage_and_cost(
            node,
            &views,
            multipliers[&node.id],
            window_ms,
            evaluation_grid_of(node.id),
            data,
            settings,
        )
        .map_err(|error| (node.id, error))?;
        if let Some(unstored) = one_batch_nodes.get(&node.id) {
            let batch = node_resource_usage_and_cost(
                unstored,
                &views,
                multipliers[&node.id],
                window_ms,
                evaluation_grid_of(node.id),
                data,
                settings,
            )
            .map_err(|error| (node.id, error))?;
            one_batch.insert(node.id, batch);
        }
        costed.insert(node.id, node_costed);
    }
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
    for node in &dag.nodes {
        let node_costed = &costed[&node.id];
        usage.cpu_ops_per_second += node_costed.cpu_ops_per_second;
        usage.scanned_bytes_per_second += node_costed.scanned_bytes_per_second;
        usage.average_memory_bytes += node_costed.average_memory_bytes;
        usage.peak_memory_bytes = usage
            .peak_memory_bytes
            .saturating_add(node_costed.peak_memory_bytes);
        usage.disk_bytes = usage.disk_bytes.saturating_add(node_costed.disk_bytes);
        fit.add(node_costed.fit);
        work_ms.insert(node.id, node_costed.work_ms_per_execution);
        output.insert(node.id, node_costed.output);
        if let Some(window) = node_costed.maintained {
            maintained.push((node.id, window));
        }
        if let Some((source, bytes)) = node_costed.raw_scan {
            let largest = raw_data.entry(source).or_insert((bytes, node.id));
            if bytes > largest.0 {
                *largest = (bytes, node.id);
            }
        }
        per_node.insert(node.id, node_costed.cost.clone());
    }
    // The runtime's fixed cost per query request, charged to the node the
    // query reads first and added to its latency below.
    let evaluation_ms = table.ns_per_evaluation * 1e-6;
    let step_ms = table.ns_per_evaluation_step * 1e-6;
    let starts = query_starts(dag);
    let mut evaluation_ms_per_query = vec![0.0; starts.len()];
    let mut request_ms_per_second = 0.0;
    // Literal-only roots have no operator to own their fee. Keep that charge
    // at plan level while preserving existing attribution for operator queries.
    let mut unattached_request_cost = 0.0;
    let mut has_unattached_request_overhead = false;
    for (query, start) in starts.iter().enumerate() {
        if evaluation_ms <= 0.0 && step_ms <= 0.0 {
            continue;
        }
        let diagnostic_node = start.first().copied().unwrap_or(first);
        let (requests_per_second, ops_per_second) = request_overhead(entries.get(query), settings)
            .map_err(|error| (diagnostic_node, error))?;
        usage.cpu_ops_per_second += ops_per_second;
        request_ms_per_second += evaluation_ms * requests_per_second;
        if !usage.cpu_ops_per_second.is_finite() || !request_ms_per_second.is_finite() {
            return Err((diagnostic_node, AnalyticalCostError::Overflow));
        }
        evaluation_ms_per_query[query] = evaluation_ms
            + step_ms
                * step_rounds(
                    steps_per_request(entries.get(query)),
                    settings.data_plane_execution.query_threads,
                ) as f64;
        let Some(&first_read) = start.first() else {
            has_unattached_request_overhead = true;
            if let Some(unit_cost) = unit_costs {
                unattached_request_cost += unit_cost.cost_per_cpu_op * ops_per_second;
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
        if let (Some(cost), Some(unit_cost)) = (&mut node.cost_per_second, unit_costs) {
            *cost += unit_cost.cost_per_cpu_op * ops_per_second;
            if !cost.is_finite() {
                return Err((first_read, AnalyticalCostError::Overflow));
            }
        }
    }
    for (bytes, id) in raw_data.into_values() {
        let node = per_node.get_mut(&id).expect("every node is measured");
        usage.disk_bytes = usage.disk_bytes.saturating_add(bytes);
        node.detail += &format!("; keeps {bytes} bytes of raw data on disk");
        if let (Some(cost), Some(disk_cost)) = (
            &mut node.cost_per_second,
            raw_data_disk_cost_per_second(bytes, settings),
        ) {
            *cost += disk_cost;
        }
    }
    // With k ingest workers, each of the k − 1 beyond the first builds its
    // own partial copy of an in-memory ingestion-time state's newest window
    // (merged at query time, [`query_latencies`]), held at the peak too.
    for &(id, window) in &maintained {
        let copies = ingest_workers(dag, &work_ms, id, settings) - 1;
        if copies == 0 {
            continue;
        }
        let bytes = copies.saturating_mul(window);
        let measured = per_node.get_mut(&id).expect("every node is measured");
        usage.peak_memory_bytes = usage.peak_memory_bytes.saturating_add(bytes);
        measured.detail +=
            &format!("; {copies} partial copies of its newest window, {bytes} bytes");
    }
    // With a window cache, a stored view keeps in memory only the windows
    // written or read within its idle time; the others are disk.
    if let Some(cache) = settings.data_plane_execution.view_window_cache {
        resident_view_windows(
            dag,
            nodes,
            &maintained,
            &producers_of,
            entries,
            cache,
            unit_costs,
            &mut usage,
            &mut per_node,
        );
    }
    let merge_ms =
        |id: PhysicalASAPNodeId| partial_copy_merge_ms(nodes[&id], &costed[&id], settings);
    let groups = refresh_groups(
        dag,
        nodes,
        &output,
        &work_ms,
        merge_ms,
        &mut per_node,
        settings,
    );
    let mut latency = query_latencies(dag, nodes, &work_ms, &groups, entries, settings)
        .map_err(|error| (first, error))?;
    for (latency, evaluation_ms) in latency.iter_mut().zip(evaluation_ms_per_query) {
        latency.query_execution_ms += evaluation_ms;
    }
    let execution = &settings.data_plane_execution;
    let refresh_cores: f64 = groups
        .iter()
        .map(|group| group.wall_ms_per_window / group.window.width_ms as f64)
        .sum();
    usage.precompute_load = refresh_cores / f64::from(execution.precompute_threads);
    let ingest_cores: f64 = per_node
        .values()
        .map(|node| node.phase_times.ingest_cpu_core_seconds_per_second)
        .sum::<f64>()
        + ingest_decode_cpu_core_seconds_per_second(data, settings);
    let query_cores: f64 = dag
        .nodes
        .iter()
        .filter(|node| node.output_state.timing == ExecutionTiming::QueryTime)
        .map(|node| work_ms[&node.id] * multipliers[&node.id].1 / 1_000.0)
        .sum::<f64>()
        + request_ms_per_second / 1_000.0;
    let utilization = execution.max_thread_utilization;
    usage.recommended_configuration = RecommendedThreads {
        ingest: RoleThreads::of(ingest_cores, utilization),
        precompute: RoleThreads::of(refresh_cores, utilization),
        query: RoleThreads::of(query_cores, utilization),
    };
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
        CostModel::ProvisionedCapacity(unit_cost) => {
            let cost = unit_cost.cost_per_provisioned_memory_byte_second
                * usage.peak_memory_bytes as f64
                + unit_cost.cost_per_provisioned_cpu_core_second * f64::from(usage.peak_cpu_cores)
                + unit_cost.cost_per_disk_byte_second * usage.disk_bytes as f64;
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

/// Moves each in-memory ingestion-time view's windows that are not
/// resident under `cache` (neither written nor read within its idle time,
/// [`resident_windows`]) from memory to disk, in `usage` and in the view's
/// node cost: `maintained` holds each such view's bytes per window. Then,
/// with `cache.max_bytes`, the resident windows past that bound are disk
/// too.
#[allow(clippy::too_many_arguments)]
fn resident_view_windows(
    dag: &PhysicalASAPDAG,
    nodes: &NodeMap<'_>,
    maintained: &[(PhysicalASAPNodeId, u64)],
    producers_of: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
    entries: &[QueryWorkloadEntry],
    cache: ViewWindowCache,
    unit_costs: Option<&ResourceUnitCost>,
    usage: &mut ResourceUsage,
    per_node: &mut BTreeMap<PhysicalASAPNodeId, NodeCost>,
) {
    // The queries whose roots reach each node.
    let mut queries_of: HashMap<PhysicalASAPNodeId, Vec<usize>> = HashMap::new();
    for (query, &root) in dag.roots.iter().enumerate() {
        let mut seen = HashSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if seen.insert(id) {
                queries_of.entry(id).or_default().push(query);
                pending.extend(producers_of.get(&id).into_iter().flatten().copied());
            }
        }
    }
    let width_of = |id: PhysicalASAPNodeId| dag.refresh_window(id).map(|window| window.width_ms);
    let mut resident_bytes_total = 0.0;
    let mut moved: Vec<(PhysicalASAPNodeId, f64, u64)> = Vec::new();
    for &(id, window_bytes) in maintained {
        let node = nodes[&id];
        let (Some(width_ms), Some(retention_ms)) = (width_of(id), node.retention_ms) else {
            continue;
        };
        let retained = retention_ms.div_ceil(width_ms.max(1));
        let readers: Vec<ChainReader> = match windows_readers_need(dag, id) {
            None => vec![ChainReader {
                read: WindowRead::Every,
                times: ReadTimes::Refresh { width_ms },
            }],
            Some(windows) => windows
                .into_iter()
                .flat_map(|(reader, window)| {
                    let reader_node = nodes[&reader];
                    let read = match &reader_node.payload {
                        Payload::NonASAP(NonASAPOp::TimeRangeCover { .. }) => {
                            let levels = &producers_of[&reader];
                            match (
                                levels.iter().position(|&level| level == id),
                                levels.iter().map(|&level| width_of(level)).collect(),
                            ) {
                                (Some(level), Some(level_widths_ms)) => WindowRead::Cover {
                                    range_ms: window.length_ms,
                                    level_widths_ms,
                                    level,
                                },
                                _ => WindowRead::Every,
                            }
                        }
                        _ => WindowRead::Range {
                            range_ms: window.length_ms,
                            offset_ms: window.ends_before_evaluation_ms,
                        },
                    };
                    let times: Vec<ReadTimes> = match reader_node.output_state.timing {
                        ExecutionTiming::IngestionTime => vec![ReadTimes::Refresh {
                            width_ms: width_of(reader).unwrap_or(width_ms),
                        }],
                        ExecutionTiming::QueryTime => queries_of
                            .get(&reader)
                            .into_iter()
                            .flatten()
                            .filter_map(|&query| entries.get(query))
                            .map(|entry| {
                                let grid = entry.evaluation_grid();
                                let range = entry.range_evaluation;
                                ReadTimes::Query {
                                    period_ms: grid.period_ms,
                                    phase_ms: grid.phase_ms,
                                    steps: range.map_or(1, |range| range.evaluations_per_request()),
                                    step_ms: range.map_or(width_ms, |range| range.step_ms),
                                }
                            })
                            .collect(),
                    };
                    times.into_iter().map(move |times| ChainReader {
                        read: read.clone(),
                        times,
                    })
                })
                .collect(),
        };
        let (mean, most) = resident_windows(width_ms, retained, cache.idle_ms, &readers);
        let mean = mean.min(retained as f64);
        let most = most.min(retained);
        let cold_bytes = window_bytes as f64 * (retained as f64 - mean);
        usage.average_memory_bytes -= cold_bytes;
        usage.peak_memory_bytes = usage
            .peak_memory_bytes
            .saturating_sub(window_bytes.saturating_mul(retained - most));
        resident_bytes_total += window_bytes as f64 * mean;
        moved.push((id, cold_bytes, window_bytes));
        let node_cost = per_node.get_mut(&id).expect("every node is measured");
        node_cost.detail += &format!(
            "; {mean:.1} of {retained} windows resident (window cache, idle {} ms)",
            cache.idle_ms
        );
    }
    // A bound on the cache: what is past it is disk too.
    let over_bound = cache.max_bytes.map_or(0.0, |max_bytes| {
        (resident_bytes_total - max_bytes as f64).max(0.0)
    });
    if over_bound > 0.0 {
        usage.average_memory_bytes -= over_bound;
        usage.peak_memory_bytes = usage.peak_memory_bytes.saturating_sub(over_bound as u64);
    }
    for (index, (id, cold_bytes, _)) in moved.iter().enumerate() {
        // The bound's excess is charged to the first view.
        let disk = cold_bytes + if index == 0 { over_bound } else { 0.0 };
        usage.disk_bytes = usage.disk_bytes.saturating_add(disk as u64);
        if let (Some(cost), Some(unit_cost)) = (
            &mut per_node
                .get_mut(id)
                .expect("every node is measured")
                .cost_per_second,
            unit_costs,
        ) {
            *cost += (unit_cost.cost_per_disk_byte_second - unit_cost.cost_per_memory_byte_second)
                * disk;
        }
    }
}

/// The runtime's fixed overhead of query `entry`'s requests: its requests
/// per second (once per evaluation of its recurrence, a range query's
/// steps in one request) and the CPU operations per second they cost,
/// `measured_operation_costs.ns_per_evaluation` each. `(0, 0)` without an
/// entry.
fn request_overhead(
    entry: Option<&QueryWorkloadEntry>,
    settings: &CostInputs,
) -> Result<(f64, f64), AnalyticalCostError> {
    let requests_per_second = evaluation_rate(
        entry.map(|entry| (&entry.recurrence, None)),
        settings.non_repeating_query_horizon_seconds,
    )?;
    let table = &settings.measured_operation_costs;
    // The fixed cost per request and per step of it.
    let evaluation_ms = (table.ns_per_evaluation
        + table.ns_per_evaluation_step * steps_per_request(entry) as f64)
        * 1e-6;
    Ok((
        requests_per_second,
        evaluation_ms / settings.resource_speed.ms_per_cpu_op * requests_per_second,
    ))
}

/// Cost model 1's cost per second of the runtime's fixed overhead per
/// query request, over every query of `entries`: what [`plan_cost`]
/// charges every plan for them, whichever nodes it has. 0 under cost
/// models 2 and 3 and when `ns_per_evaluation` is 0.
pub fn request_overhead_cost_per_second(
    entries: &[QueryWorkloadEntry],
    settings: &CostInputs,
) -> Result<f64, AnalyticalCostError> {
    let Some(unit_cost) = settings.cost_model.per_node_unit_costs() else {
        return Ok(0.0);
    };
    let table = &settings.measured_operation_costs;
    if table.ns_per_evaluation <= 0.0 && table.ns_per_evaluation_step <= 0.0 {
        return Ok(0.0);
    }
    let mut cost = 0.0;
    for entry in entries {
        cost += unit_cost.cost_per_cpu_op * request_overhead(Some(entry), settings)?.1;
    }
    match cost.is_finite() {
        true => Ok(cost),
        false => Err(AnalyticalCostError::Overflow),
    }
}

/// Cost model 1's cost per second of keeping `bytes` of raw data on disk;
/// `None` under cost models 2 and 3. A plan keeps each source once, for its
/// scan that keeps the most ([`plan_cost`]).
pub fn raw_data_disk_cost_per_second(bytes: u64, settings: &CostInputs) -> Option<f64> {
    settings
        .cost_model
        .per_node_unit_costs()
        .map(|unit_cost| unit_cost.cost_per_disk_byte_second * bytes as f64)
}

/// The window a query-time `Scan` `node` of `dag` reads per execution: the
/// most offset + range its consumers read ([`read_back_ms`], no
/// range-evaluation span), once however many consumers read it. `None` for
/// any other node, or when a consumer reads it whole.
fn scan_read_ms(dag: &PhysicalASAPDAG, node: &PhysicalASAPDAGNode) -> Option<u64> {
    if !matches!(node.payload, Payload::NonASAP(NonASAPOp::Scan { .. })) {
        return None;
    }
    read_back_ms(dag, node.id, |_| 0)
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
    let evaluating = latency
        .iter()
        .filter(|l| l.query_execution_ms > 0.0)
        .count() as u32;
    cores(ingestion_ms_per_second) + evaluating.max(cores(query_ms_per_second))
}

/// Milliseconds of one merge of `node`'s state (with its `usage`) when the
/// newest window's partial copies are compacted: its measured query-time
/// merge entry (ns · 10⁻⁶, at its series count) when the table has one,
/// else the merge formula of its family and bytes per state, as a
/// `SummaryMerge` over it is costed (a DDSketch state: 2,500 operations).
/// 0 for a node holding no states, such as the raw rows a `Filter` or
/// `TimeRange` keeps or an exact `Aggregate`'s values: the executor merges
/// ingest shards only of summary state columns, and partial copies of rows
/// are concatenated. No per-group term (`merge_output_terms`, 19,000 per
/// KLL group): the executor merges a shard's states into the existing
/// states in place, group by group, and creates no group's state or output
/// row.
pub fn partial_copy_merge_ms(
    node: &PhysicalASAPDAGNode,
    usage: &NodeResourceUsageAndCost<'_>,
    settings: &CostInputs,
) -> f64 {
    let family = usage.family;
    if family == StateFamily::None {
        return 0.0;
    }
    let sketch_size =
        |params: &SketchParams| sketch_state_bytes(params, StateInputRows::UNKNOWN) as f64;
    lookup_at_scale(
        &settings.measured_operation_costs,
        OperatorKind::SummaryMerge,
        ExecutionTiming::QueryTime,
        family,
        sketch_size,
        usage.series,
    )
    .map_or_else(
        || {
            // A partial copy merges into a state of the same window.
            let state_bytes = state_bytes_per_row(usage.output, &node.output_schema);
            merge_terms_per_state(family, state_bytes, state_bytes, None, 1).operations()
                * settings.resource_speed.ms_per_cpu_op
                * settings.frozen_calibration_factor.unwrap_or(1.0)
        },
        |unit| (unit.ns_per_unit + unit.ns_per_invocation) * 1e-6,
    )
}

/// The ingest threads that each build a partial copy of an in-memory
/// ingestion-time state's newest window, for a state whose ingestion work
/// takes `ingest_cores`: the deployment's `ingest_threads` when set, else
/// one per core that work takes, at least one.
pub fn ingest_threads_building_copies(settings: &CostInputs, ingest_cores: f64) -> u64 {
    match settings.data_plane_execution.ingest_threads {
        Some(threads) => u64::from(threads),
        None => ingest_cores.ceil().max(1.0) as u64,
    }
}

/// Steps one request of `entry` evaluates: its range evaluation's, 1
/// without one (or without an entry).
/// The rounds a request's `steps` take when they split over
/// `query_threads` query threads: ⌈steps ÷ threads⌉, the latency's
/// multiplier of per-step work (its CPU stays `steps` times).
pub fn step_rounds(steps: u64, query_threads: u32) -> u64 {
    steps.div_ceil(u64::from(query_threads.max(1)))
}

fn steps_per_request(entry: Option<&QueryWorkloadEntry>) -> u64 {
    entry
        .and_then(|entry| entry.range_evaluation)
        .map_or(1, |range| range.evaluations_per_request())
}

/// Core-seconds per second of the decode and append of arriving samples
/// outside every plan node (`MeasuredOperationCosts::ns_per_ingested_sample`
/// × the data's arriving rows per second); 0 when either is unknown.
pub fn ingest_decode_cpu_core_seconds_per_second(
    data: &DataWorkload,
    settings: &CostInputs,
) -> f64 {
    data.arrival.rows_per_second().unwrap_or(0.0)
        * settings.measured_operation_costs.ns_per_ingested_sample
        * 1e-9
}

/// One refresh group of a plan: its ingestion-time nodes with one refresh
/// window, refreshed together once per window close on one precompute
/// thread (`PhysicalASAPDAG::ingestion_refresh_groups`).
#[derive(Debug, Clone)]
pub(crate) struct RefreshGroup {
    pub(crate) window: RefreshWindow,
    pub(crate) nodes: Vec<PhysicalASAPNodeId>,
    /// Wall ms of one refresh: its nodes' refresh wall time, the merging
    /// of the ingest threads' partial copies, and the I/O wait per run.
    pub(crate) wall_ms_per_window: f64,
    /// The other groups whose nodes its nodes read, by position.
    pub(crate) reads: BTreeSet<usize>,
}

/// Whether windows `a` and `b` can close at one instant: their phases agree
/// modulo the gcd of their widths.
pub fn close_together(a: RefreshWindow, b: RefreshWindow) -> bool {
    let gcd = greatest_common_divisor(a.width_ms, b.width_ms).max(1);
    a.phase_ms % gcd == b.phase_ms % gcd
}

/// The plan's refresh groups ([`RefreshGroup`]). Each ingestion-time node
/// with a refresh window joins its window's group; the merging of the
/// ingest threads' `(k − 1)·I` partial copies of its `I` states (`merge_ms`
/// each) is refresh work of that node, and each group's run pays
/// `refresh_io_wait.ns_per_refresh` once, on its first node. Both are added
/// to the nodes' `phase_times` in `per_node`.
fn refresh_groups(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode>,
    output: &HashMap<PhysicalASAPNodeId, EdgeStatistics>,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    merge_ms: impl Fn(PhysicalASAPNodeId) -> f64,
    per_node: &mut BTreeMap<PhysicalASAPNodeId, NodeCost>,
    settings: &CostInputs,
) -> Vec<RefreshGroup> {
    let mut groups: Vec<RefreshGroup> = Vec::new();
    let mut group_of: HashMap<PhysicalASAPNodeId, usize> = HashMap::new();
    let wait_per_run_ms = settings
        .measured_operation_costs
        .refresh_io_wait
        .ns_per_refresh
        * 1e-6;
    for node in &dag.nodes {
        if nodes[&node.id].output_state.timing != ExecutionTiming::IngestionTime {
            continue;
        }
        let measured = per_node.get_mut(&node.id).expect("every node is measured");
        // A refresh group refreshes views, and the nodes doing refresh work.
        let refreshes = node.is_view() || measured.phase_times.refresh_cpu_ms_per_window > 0.0;
        if measured.phase_times.refresh_window_width_ms.is_none() || !refreshes {
            continue;
        }
        let Some(window) = dag.refresh_window(node.id) else {
            continue;
        };
        let workers = ingest_workers(dag, work_ms, node.id, settings) as f64;
        let compaction_ms = (workers - 1.0) * output[&node.id].rows as f64 * merge_ms(node.id);
        measured.phase_times.refresh_cpu_ms_per_window += compaction_ms;
        measured.phase_times.refresh_wall_ms_per_window += compaction_ms;
        let group = match groups.iter().position(|group| group.window == window) {
            Some(group) => group,
            None => {
                measured.phase_times.refresh_wall_ms_per_window += wait_per_run_ms;
                groups.push(RefreshGroup {
                    window,
                    nodes: Vec::new(),
                    wall_ms_per_window: 0.0,
                    reads: BTreeSet::new(),
                });
                groups.len() - 1
            }
        };
        groups[group].nodes.push(node.id);
        groups[group].wall_ms_per_window += measured.phase_times.refresh_wall_ms_per_window;
        group_of.insert(node.id, group);
    }
    for edge in &dag.edges {
        if let (Some(&consumer), Some(&producer)) =
            (group_of.get(&edge.consumer), group_of.get(&edge.producer))
        {
            if consumer != producer {
                groups[consumer].reads.insert(producer);
            }
        }
    }
    groups
}

/// The freshness wait of each group beyond the allowed lateness, as
/// (queueing, execution) in ms: execution its own refresh wall time;
/// queueing the groups it reads in full, plus the other groups closing at
/// the same instant divided among `precompute_threads` (the v1 linear
/// upper bound).
pub(crate) fn group_waits(groups: &[RefreshGroup], precompute_threads: u32) -> Vec<(f64, f64)> {
    groups
        .iter()
        .enumerate()
        .map(|(position, group)| {
            let mut queueing = 0.0;
            for (other_position, other) in groups.iter().enumerate() {
                if other_position == position {
                    continue;
                }
                if group.reads.contains(&other_position) {
                    queueing += other.wall_ms_per_window;
                } else if close_together(group.window, other.window) {
                    queueing += other.wall_ms_per_window / f64::from(precompute_threads);
                }
            }
            (queueing, group.wall_ms_per_window)
        })
        .collect()
}

/// Each query's latency per request ([`QueryLatency`]), without its fixed
/// cost per request and step (added by [`plan_cost`]): the query-time work
/// it reaches, each node once per step of the request (once per request
/// for a node kept across evaluations), after the allowed lateness and the longest
/// freshness wait among the refresh groups of the ingestion-time nodes it
/// reads.
fn query_latencies(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &PhysicalASAPDAGNode>,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    groups: &[RefreshGroup],
    entries: &[QueryWorkloadEntry],
    settings: &CostInputs,
) -> Result<Vec<QueryLatency>, AnalyticalCostError> {
    let execution = &settings.data_plane_execution;
    let waits = group_waits(groups, execution.precompute_threads);
    let group_of: HashMap<PhysicalASAPNodeId, usize> = groups
        .iter()
        .enumerate()
        .flat_map(|(position, group)| group.nodes.iter().map(move |&id| (id, position)))
        .collect();
    let inputs = |id: PhysicalASAPNodeId| {
        dag.edges
            .iter()
            .filter(move |e| e.consumer == id)
            .map(|e| e.producer)
    };
    query_starts(dag)
        .into_iter()
        .enumerate()
        .map(|(query, start)| {
            let mut latency = QueryLatency {
                allowed_lateness_ms: execution.allowed_lateness_ms as f64,
                evaluations_per_second: evaluation_rate(
                    entries
                        .get(query)
                        .map(|entry| (&entry.recurrence, entry.range_evaluation)),
                    settings.non_repeating_query_horizon_seconds,
                )?,
                ..QueryLatency::default()
            };
            let mut seen = HashSet::new();
            let mut stack = start;
            while let Some(id) = stack.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if nodes[&id].output_state.timing == ExecutionTiming::IngestionTime {
                    if let Some(&group) = group_of.get(&id) {
                        let (queueing, refresh) = waits[group];
                        if queueing + refresh
                            > latency.precompute_queueing_ms + latency.precompute_execution_ms
                        {
                            latency.precompute_queueing_ms = queueing;
                            latency.precompute_execution_ms = refresh;
                        }
                    }
                    continue;
                }
                // Computed once per step of the request, once per request
                // when kept across evaluations (as `computations` counts it).
                let per_request = match nodes[&id].lifetime {
                    Lifetime::AcrossEvaluations(_) => 1,
                    Lifetime::None | Lifetime::OneEvaluation(_) => step_rounds(
                        steps_per_request(entries.get(query)),
                        settings.data_plane_execution.query_threads,
                    ),
                };
                latency.query_execution_ms += work_ms[&id] * per_request as f64;
                stack.extend(inputs(id));
            }
            Ok(latency)
        })
        .collect()
}

/// The ingest threads building partial copies of `state`'s newest window:
/// the deployment's `ingest_threads` when set, else the cores `state`'s
/// ingestion work takes, at least one.
fn ingest_workers(
    dag: &PhysicalASAPDAG,
    work_ms: &HashMap<PhysicalASAPNodeId, f64>,
    state: PhysicalASAPNodeId,
    settings: &CostInputs,
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
    ingest_threads_building_copies(settings, ingest_ms_per_s / 1_000.0)
}

/// The nodes of `dag` by id.
fn node_map(dag: &PhysicalASAPDAG) -> NodeMap<'_> {
    dag.nodes.iter().map(|n| (n.id, n)).collect()
}

/// The nodes of a physical DAG by id.
type NodeMap<'a> = HashMap<PhysicalASAPNodeId, &'a PhysicalASAPDAGNode>;
