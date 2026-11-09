//! Costing a physical plan DAG node by node: each node's resource usage
//! ([`node_resource_usage_and_cost`]), then the plan's usage, latency, retention, retained tumbling window
//! chains and cost ([`plan_cost`]). Moved from Stage 3 (`asap-stage3-plan-selection`,
//! whose module doc, "Cost model", describes the model), so Stage 3 computes
//! no cost of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::analytical::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::execution_demand::{computations, evaluation_rate, query_starts};
use crate::operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::read_back::read_back_ms;
use crate::state_bytes::{
    keeps_every_value, row_bytes, sketch_shape, summary_shape, StateInputRows,
};
use crate::{aggregate_lookup, lookup, StateFamily};
use crate::{
    CalibrationFit, CostInputs, CostModel, NodeCost, NodeCostSource, OperatorKind, PlanCost,
    QueryLatency, ResourceUsage, COST_SOURCE, MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
use asap_types::ir::operator::{GroupKeys, Reduction, Source};
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{FieldDataType, Schema, SketchParams, SketchStatistic};
use asap_types::ir::{ASAPOp, NonASAPOp, TimeRangeKind};
use asap_types::physical::{Lifetime, StorageMedium};
use asap_types::workload::{DataWorkload, QueryRecurrence, QueryWorkloadEntry, RangeEvaluation};

use asap_types::workload::bounds::{
    declared_non_negative, declared_series, group_count_bound, source_name, source_rows_per_ms,
    unchanged_source_columns,
};

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
    let EachNodeUsage {
        priced,
        multipliers,
    } = each_node_usage(
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

/// Every node of a physical DAG priced ([`each_node_usage`]), with each
/// node's `(multiplier, per_second)` ([`computations`]).
struct EachNodeUsage<'a> {
    priced: HashMap<PhysicalASAPNodeId, NodeResourceUsageAndCost<'a>>,
    multipliers: HashMap<PhysicalASAPNodeId, (u64, f64)>,
}

/// [`node_resource_usage_and_cost`] over every node of `dag`, children first, with the
/// arguments of [`plan_cost`]. `root_rates`, when given, are
/// the computations per second of each root's consumer in place of its
/// query's own rate ([`computations`]).
#[allow(clippy::too_many_arguments)]
fn each_node_usage<'a>(
    dag: &'a PhysicalASAPDAG,
    nodes: &NodeMap<'a>,
    recurrence: &[QueryRecurrence],
    range_evaluations: &[Option<RangeEvaluation>],
    data: &DataWorkload,
    settings: &'a CostInputs,
    formula_scale: f64,
    root_rates: Option<&[f64]>,
) -> Result<EachNodeUsage<'a>, (PhysicalASAPNodeId, AnalyticalCostError)> {
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
                    chain_tumbling_window_width_ms: produced.chain_tumbling_window_width_ms,
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
    Ok(EachNodeUsage {
        priced,
        multipliers,
    })
}

/// The window a query-time `Scan` `node` reads per execution: the most
/// offset + range its consumers read ([`read_back_ms`], no range-evaluation
/// span), once however many consumers read it. `None` for any other node,
/// or when a consumer reads it whole.
fn scan_read_ms(
    node: &asap_types::ir::physical_export::PhysicalASAPDAGNode,
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
        |id| matches!(nodes[&id].lifetime, Lifetime::AcrossEvaluations(_)),
        true,
    )
}

/// The state family of `node` whose first producer has `producer_family`:
/// a build's own, its θ (`node.parameters`; one sketch instance per group;
/// a shared Hydra structure is not that sketch), inherited by the merges,
/// estimates and finalizations over it, none otherwise. Err
/// [`AnalyticalCostError::UnsupportedCandidate`] for a sketch build without
/// θ, a planner bug.
fn state_family<'a>(
    node: &'a asap_types::ir::physical_export::PhysicalASAPDAGNode,
    producer_family: Option<StateFamily<'a>>,
) -> Result<StateFamily<'a>, AnalyticalCostError> {
    Ok(match &node.payload {
        Payload::ASAP(
            ASAPOp::SummaryAgg {
                summary_type: family,
                ..
            }
            | ASAPOp::SummaryTumblingWindowChain {
                summary_type: family,
                ..
            },
        ) => match family {
            FieldDataType::Sketch(..) => node
                .parameters
                .as_ref()
                .map(StateFamily::Sketch)
                .ok_or(AnalyticalCostError::UnsupportedCandidate)?,
            FieldDataType::ExactAggregate(kind, _) => StateFamily::Exact(kind),
            _ => StateFamily::None,
        },
        Payload::ASAP(
            ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. },
        ) => producer_family.unwrap_or(StateFamily::None),
        _ => StateFamily::None,
    })
}

/// One producer of a node being priced ([`node_resource_usage_and_cost`]): its payload and
/// schema, its output statistics, the sources it reads and its state
/// family.
#[derive(Debug, Clone)]
pub struct ProducerOutput<'a> {
    pub payload: &'a Payload,
    pub output_schema: &'a Schema,
    pub output: EdgeStatistics,
    pub sources: BTreeSet<&'a str>,
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
    /// The tumbling windows its rows span: those a `TimeRange` selects
    /// from a chain (through `TimeShift`s), 1 for any other node.
    pub tumbling_windows: u64,
    /// The tumbling-window width of the chain whose states its rows are
    /// read from: a chain's own, kept through the `TimeShift`s over it,
    /// so a `TimeRange` over them selects windows; `None` for any other
    /// node.
    pub chain_tumbling_window_width_ms: Option<u64>,
}

/// What one node uses and outputs ([`node_resource_usage_and_cost`]): its output statistics, sources and state family for its
/// consumers; its work for one computation (one execution) in ms on one
/// core; its usage per second (query-time memory held while it computes
/// included); what is charged once the whole plan is known (an in-memory
/// ingestion window's partial copies, the raw data a query-time scan keeps
/// on disk); its calibration fit and its [`NodeCost`].
#[derive(Debug, Clone)]
pub struct NodeResourceUsageAndCost<'a> {
    pub output: EdgeStatistics,
    pub sources: BTreeSet<&'a str>,
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
    /// The tumbling windows its rows span: those a `TimeRange` selects
    /// from a chain (through `TimeShift`s), 1 for any other node.
    pub tumbling_windows: u64,
    /// The tumbling-window width of the chain whose states its rows are
    /// read from: a chain's own, kept through the `TimeShift`s over it,
    /// so a `TimeRange` over them selects windows; `None` for any other
    /// node.
    pub chain_tumbling_window_width_ms: Option<u64>,
    pub work_ms_per_execution: f64,
    pub cpu_ops_per_second: f64,
    pub scanned_bytes_per_second: f64,
    pub average_memory_bytes: f64,
    pub peak_memory_bytes: u64,
    pub disk_bytes: u64,
    /// An in-memory ingestion-time state's bytes per window.
    pub maintained: Option<u64>,
    /// A query-time scan's source and the raw bytes it reads.
    pub raw_scan: Option<(&'a Source, u64)>,
    pub fit: CalibrationFit,
    pub cost: NodeCost,
}

/// Measure and price one node of a physical DAG (one step of
/// [`plan_cost`]): `producers` are its producers in edge order,
/// `rate` its `(multiplier, per_second)` ([`computations`]), and
/// `window_ms` the time window one execution covers: the window an
/// ingestion-time node maintains, or a query-time `Scan`'s read, the most
/// offset + range its consumers read ([`read_back_ms`] without a span;
/// `None`: it reads what it retains)
/// (`maintained_window_ms`). A materialized node keeps what its
/// `retention_ms` ($h_v$) covers: a tumbling-window chain its windows in
/// it, a query-time `Scan` and raw rows materialized at ingestion time (a
/// `Scan`, or a `TimeShift` by an offset over one) their raw rows in it.
#[allow(clippy::too_many_arguments)]
pub fn node_resource_usage_and_cost<'a>(
    node: &'a asap_types::ir::physical_export::PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    rate: (u64, f64),
    window_ms: Option<u64>,
    data: &DataWorkload,
    settings: &'a CostInputs,
    formula_scale: f64,
) -> Result<NodeResourceUsageAndCost<'a>, AnalyticalCostError> {
    let table = &settings.measured_operation_costs;
    let speed = settings.resource_speed;
    let usage_prices = match &settings.cost_model {
        CostModel::UsageOverTime(prices) => Some(prices),
        _ => None,
    };
    let sketch_size =
        |params: &SketchParams| sketch_shape(params, StateInputRows::UNKNOWN).1 as f64;
    let mut fit = CalibrationFit::default();
    // Query-time memory held while the node computes.
    let mut held_bytes = None;
    let mut maintained = None;
    let mut raw_scan = None;
    // A query-time scan's raw rows retained.
    let mut raw_retained_rows = None;
    let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
    // The completed tumbling windows a chain retains: its `retention_ms`
    // over its window width, at least one.
    let retained_tumbling_window_count = match &node.payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => node
            .retention_ms
            .map_or(1, |retention_ms| {
                retention_ms.div_ceil((*tumbling_window_width_ms).max(1))
            })
            .max(1),
        _ => 1,
    };
    let inputs: Vec<EdgeStatistics> = producers.iter().map(|p| p.output).collect();
    let sources: BTreeSet<&str> = match &node.payload {
        Payload::NonASAP(NonASAPOp::Scan { source, .. }) => [source_name(source)].into(),
        _ => producers
            .iter()
            .flat_map(|p| p.sources.iter().copied())
            .collect(),
    };
    // The series its sources declare, when they all do.
    let series = declared_series(&sources, data);
    // The rows per ms its sources receive, which its scans and time
    // ranges read, when the data declares an arrival rate.
    let rows_per_ms = source_rows_per_ms(&sources, data);
    // Raw rows materialized at ingestion time: a `Scan`, or a `TimeShift`
    // by an offset over one, across evaluations. It is a time-indexed store
    // its consumers read back in, retaining `retention_ms` ($h_v$) of rows.
    let raw_rows_materialized_at_ingestion = ingestion
        && matches!(node.lifetime, Lifetime::AcrossEvaluations(_))
        && match &node.payload {
            Payload::NonASAP(NonASAPOp::Scan { .. }) => true,
            Payload::NonASAP(NonASAPOp::TimeShift { shift, .. }) => {
                shift.at.is_none()
                    && producers.first().is_some_and(|producer| {
                        matches!(producer.payload, Payload::NonASAP(NonASAPOp::Scan { .. }))
                    })
            }
            _ => false,
        };
    // The raw rows of its sources back `lookback_ms` (every row held
    // without one): by arrival rate, at most the rows they hold.
    let retained_raw_rows = |lookback_ms: Option<u64>| -> Option<u64> {
        let by_rate = rows_per_ms
            .zip(lookback_ms)
            .map(|(rows_per_ms, lookback_ms)| (rows_per_ms * lookback_ms as f64).round() as u64);
        let rows_held: Option<u64> = sources
            .iter()
            .map(|source| data.sources.get(*source).and_then(|facts| facts.rows_held))
            .sum();
        match (by_rate, rows_held) {
            (Some(by_rate), Some(rows_held)) => Some(by_rate.min(rows_held)),
            (by_rate, rows_held) => by_rate.or(rows_held),
        }
        .map(|rows| rows.max(1))
    };
    // The fact a rate needs, named when it is missing.
    let retained_without_a_bound = |source: &str| AnalyticalCostError::MissingSourceFact {
        source_name: source.to_string(),
        missing: "its rows_held (raw data retained without a bound)",
    };
    let missing_rate = |source: &str| AnalyticalCostError::MissingSourceFact {
        source_name: source.to_string(),
        missing: "an arrival rate (the arrival's rows_per_second, or the \
                  source's series_count with sample_interval)",
    };
    let family = state_family(node, producers.first().map(|p| p.family))?;
    let unchanged_columns = unchanged_source_columns(
        node,
        &producers
            .iter()
            .map(|p| (p.output_schema, &p.unchanged_columns))
            .collect::<Vec<_>>(),
    );
    let operator = OperatorKind::of(&node.payload);
    let unit_price = match &node.payload {
        Payload::NonASAP(NonASAPOp::Aggregate { measures, .. }) => {
            aggregate_lookup(table, node.output_state.timing, measures)
        }
        _ => lookup(
            table,
            operator,
            node.output_state.timing,
            family,
            sketch_size,
        ),
    };
    // The kernel's measured bytes per state, when the table has them.
    let measured_state_bytes = unit_price
        .and_then(|p| p.memory_bytes_per_state)
        .map(|bytes| bytes.round() as u64);
    let mut cost_source = NodeCostSource::Analytical;
    let input = inputs
        .first()
        .copied()
        .unwrap_or(EdgeStatistics { rows: 1, bytes: 1 });
    let width = row_bytes(&node.output_schema);
    let edge = |rows: u64| -> Result<EdgeStatistics, AnalyticalCostError> {
        Ok(EdgeStatistics {
            rows,
            bytes: rows
                .checked_mul(width)
                .ok_or(AnalyticalCostError::Overflow)?,
        })
    };
    // Whether a summary built here sees raw data samples (its producer a
    // scan or a time range) of sources all declared non-negative.
    let raw_data_samples_non_negative = || {
        let raw = producers.first().is_some_and(|producer| {
            matches!(
                producer.payload,
                Payload::NonASAP(NonASAPOp::Scan { .. } | NonASAPOp::TimeRange { .. })
            )
        });
        raw && sources
            .iter()
            .all(|source| declared_non_negative(data, source))
    };
    let unary = |output| UnaryEdgeStatistics {
        input,
        output,
        promql: None,
    };
    // The schema the node's group and partition keys index.
    let input_schema = producers.first().map(|p| p.output_schema);
    // The rows a grouping reads per evaluation: its input at query
    // time; at ingestion time, the arriving rows of the window it
    // maintains (one second of them does not bound its groups).
    // Query time always has its input rows; an ingestion-time grouping
    // with no window below it is an error (Stage 2 builds none).
    let rows_read = || match ingestion {
        false => Ok(input.rows.max(1)),
        true => {
            let window_ms = window_ms.ok_or(AnalyticalCostError::IngestionGroupingWithoutWindow)?;
            // Name a source with no rate of its own.
            let rows_per_ms = rows_per_ms.ok_or_else(|| {
                let unrated = sources
                    .iter()
                    .find(|source| source_rows_per_ms(&[**source].into(), data).is_none());
                missing_rate(unrated.copied().unwrap_or("(none)"))
            })?;
            Ok(((rows_per_ms * window_ms as f64).round() as u64).max(1))
        }
    };
    let estimated_groups = |keys: &GroupKeys| {
        let rows_read = rows_read()?;
        Ok(group_count_bound(
            keys,
            input_schema,
            &sources,
            producers
                .first()
                .map_or(&BTreeSet::new(), |producer| &producer.unchanged_columns),
            data,
            rows_read,
        ))
    };
    let groups = |reduction: &Reduction| match reduction {
        Reduction::Reduce(keys) if keys.keys().is_empty() && !keys.is_without() => Ok(1),
        Reduction::Reduce(keys) => estimated_groups(keys),
        // One group per series: the declared series (at query time at
        // most the rows read), else the rows read, each series having at
        // least one.
        Reduction::PerEntity => match (ingestion, series) {
            (true, Some(series)) => Ok(series.max(1)),
            (false, Some(series)) => Ok(series.min(input.rows).max(1)),
            (_, None) => rows_read(),
        },
    };
    let (out, estimate, detail) = match &node.payload {
        Payload::NonASAP(operator) => match operator {
            NonASAPOp::Scan { source, .. } => {
                // At ingestion time, one second of arriving rows; at
                // query time, the arriving rows of the window read above
                // it, at most the rows the source holds (all of them
                // without a window).
                let source = source_name(source);
                let rows_held = data.sources.get(source).and_then(|facts| facts.rows_held);
                let (window_width_ms, retained_lookback_ms): (Option<u64>, Option<u64>) =
                    match ingestion {
                        true => (None, None),
                        // One execution reads the most its consumers read
                        // (`window_ms`), else what the scan retains (its
                        // `retention_ms`, $h_v$); `None` reads every row
                        // held. It keeps `retention_ms`.
                        false => (window_ms.or(node.retention_ms), node.retention_ms),
                    };
                let lookback = match ingestion {
                    true => Some(1_000),
                    false => window_width_ms,
                };
                let by_rate = |lookback: Option<u64>| {
                    rows_per_ms.zip(lookback).map(|(rows_per_ms, lookback)| {
                        (rows_per_ms * lookback as f64).round() as u64
                    })
                };
                // The raw rows retained for a query-time scan: back to the
                // oldest row it reads, `end_offset + width` (an offset
                // window keeps more than it reads), at most those held.
                // So are those an ingestion-time scan materialized across
                // evaluations keeps: back `retention_ms` ($h_v$).
                let retained_lookback_ms = match ingestion {
                    true => node.retention_ms,
                    false => retained_lookback_ms,
                };
                if !ingestion || raw_rows_materialized_at_ingestion {
                    raw_retained_rows = retained_raw_rows(retained_lookback_ms);
                }
                let by_rate = by_rate(lookback);
                let rows = match (ingestion, by_rate, rows_held) {
                    (true, by_rate, _) => by_rate.ok_or_else(|| missing_rate(source))?,
                    (false, Some(by_rate), Some(rows_held)) => by_rate.min(rows_held),
                    (false, by_rate, rows_held) => by_rate.or(rows_held).ok_or_else(|| {
                        AnalyticalCostError::MissingSourceFact {
                            source_name: source.to_string(),
                            missing: match lookback {
                                Some(_) => {
                                    "an arrival rate (the arrival's rows_per_second, or the \
                                     source's series_count with sample_interval) or its \
                                     rows_held"
                                }
                                None => "its rows_held (the scan has no window, so it reads every row held)",
                            },
                        }
                    })?,
                };
                let out = edge(rows.max(1))?;
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
                // Raw data materialized at ingestion time: its consumers
                // read the rows it retains, not one second of them.
                let out = match raw_rows_materialized_at_ingestion {
                    true => edge(raw_retained_rows.ok_or(retained_without_a_bound(source))?)?,
                    false => out,
                };
                (out, estimate, format!("scan {} samples", out.rows))
            }
            NonASAPOp::Aggregate {
                reduction,
                measures,
                ..
            } => {
                let group_count = groups(reduction)?;
                let keys = match reduction {
                    Reduction::Reduce(keys) => keys.keys().len() as u64,
                    _ => 1,
                };
                let out = edge(group_count)?;
                let held = input.rows.div_ceil(group_count.max(1));
                let estimate = estimate_operator(
                    PhysicalOperator::HashAggregate {
                        grouping_key_count: keys,
                        accumulator_count: measures.len().max(1) as u64,
                    },
                    OperatorStatistics::HashAggregate {
                        edges: unary(out),
                        group_count,
                        key_bytes: keys.checked_mul(16).ok_or(AnalyticalCostError::Overflow)?,
                        accumulator_bytes_per_group: measures
                            .iter()
                            .try_fold(0_u64, |total, measure| {
                                let values = if keeps_every_value(measure) { held } else { 1 };
                                total
                                    .checked_add(
                                        values
                                            .checked_mul(8)
                                            .ok_or(AnalyticalCostError::Overflow)?,
                                    )
                                    .ok_or(AnalyticalCostError::Overflow)
                            })?
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
                    estimated_groups(partition_by)?
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
                    estimated_groups(partition_by)?
                };
                let limit = n.map_or(u64::MAX, |n| (n as u64).saturating_mul(partitions));
                let offset = (*offset as u64).saturating_mul(partitions);
                let out = edge(input.rows.saturating_sub(offset).min(limit))?;
                let estimate = estimate_operator(
                    PhysicalOperator::Limit { limit, offset },
                    OperatorStatistics::Limit { edges: unary(out) },
                );
                (out, estimate, format!("limit to {} rows", out.rows))
            }
            NonASAPOp::TimeShift { .. } => {
                // Raw rows materialized at ingestion time: like a
                // materialized `Scan`'s, its consumers read the rows it
                // retains, not one second of them.
                let out = match raw_rows_materialized_at_ingestion {
                    true => {
                        raw_retained_rows = retained_raw_rows(node.retention_ms);
                        let source = sources.first().copied().unwrap_or("(none)");
                        edge(raw_retained_rows.ok_or(retained_without_a_bound(source))?)?
                    }
                    false => input,
                };
                (
                    out,
                    estimate_operator(
                        PhysicalOperator::TimeShift,
                        OperatorStatistics::PassThrough {
                            edges: unary(input),
                        },
                    ),
                    format!("time shift: pass {} rows, free", input.rows),
                )
            }
            NonASAPOp::TimeRange { range, .. }
                if chain_tumbling_window_width_ms(producers).is_some() =>
            {
                // Over a chain, the windows inside the range: one
                // window's states per window.
                let windows = selected_tumbling_windows(range, producers)?;
                let out = EdgeStatistics {
                    rows: input
                        .rows
                        .checked_mul(windows)
                        .ok_or(AnalyticalCostError::Overflow)?,
                    bytes: input
                        .bytes
                        .checked_mul(windows)
                        .ok_or(AnalyticalCostError::Overflow)?,
                };
                // It reads those windows' states from the chain's store.
                let estimate = estimate_operator(
                    PhysicalOperator::TimeRange,
                    OperatorStatistics::PassThrough {
                        edges: UnaryEdgeStatistics {
                            input: out,
                            output: out,
                            promql: None,
                        },
                    },
                );
                (
                    out,
                    estimate,
                    format!("time range {range:?}: {windows} tumbling windows of states"),
                )
            }
            NonASAPOp::TimeRange { range, kind, .. } => {
                // A range keeps its window's samples; an instant
                // selector the latest sample of each series.
                // Unknown rate or series: no fewer than its input.
                let retained = match kind {
                    TimeRangeKind::Range => rows_per_ms.map_or(input.rows, |rows_per_ms| {
                        (rows_per_ms * range.as_millis() as f64).round() as u64
                    }),
                    TimeRangeKind::Instant => series.unwrap_or(input.rows),
                };
                let out = edge(input.rows.min(retained.max(1)))?;
                let out = EdgeStatistics {
                    rows: out.rows,
                    bytes: out.bytes.min(input.bytes),
                };
                let estimate = estimate_operator(
                    PhysicalOperator::TimeRange,
                    OperatorStatistics::PassThrough { edges: unary(out) },
                );
                (
                    out,
                    estimate,
                    format!(
                        "time range {range:?}: keep {} of {} rows",
                        out.rows, input.rows
                    ),
                )
            }
            _ => {
                let estimate = estimate_operator(
                    PhysicalOperator::PassThrough,
                    OperatorStatistics::PassThrough {
                        edges: unary(input),
                    },
                );
                (
                    input,
                    estimate,
                    format!("operator: pass {} rows", input.rows),
                )
            }
        },
        Payload::ASAP(ASAPOp::SummaryAgg {
            summary_type: family,
            reduction,
            ..
        }) => {
            let group_count = groups(reduction)?;
            // At query time, each state sees its group's input rows; at
            // ingestion time, the rows of a window one second of
            // arriving rows does not bound.
            let input_rows = StateInputRows {
                rows_per_state: (!ingestion).then(|| input.rows.div_ceil(group_count.max(1))),
                non_negative: raw_data_samples_non_negative(),
            };
            let (depth, state_bytes) = summary_shape(family, node.parameters.as_ref(), input_rows)?;
            let state_bytes = measured_state_bytes.unwrap_or(state_bytes);
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(state_bytes)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(
                    input.rows as f64 * depth,
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} into {group_count} states: {} rows x depth {depth}",
                    summary_type_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            summary_type: family,
            reduction,
            tumbling_window_width_ms,
            ..
        }) => {
            // One window's states: each input row updates the window its
            // time falls in. At ingestion time the rows of one window
            // arrive at the rate of one second's `input.rows`; at query
            // time each build reads the rows of the window that just ended,
            // its input.
            let group_count = groups(reduction)?;
            let rows_per_tumbling_window = match ingestion {
                true => u64::try_from(
                    (u128::from(input.rows) * u128::from(*tumbling_window_width_ms))
                        .div_ceil(1_000),
                )
                .map_err(|_| AnalyticalCostError::Overflow)?,
                false => input.rows,
            };
            let input_rows = StateInputRows {
                rows_per_state: Some(rows_per_tumbling_window.div_ceil(group_count.max(1))),
                non_negative: raw_data_samples_non_negative(),
            };
            let (depth, state_bytes) = summary_shape(family, node.parameters.as_ref(), input_rows)?;
            let state_bytes = measured_state_bytes.unwrap_or(state_bytes);
            let out = EdgeStatistics {
                rows: group_count,
                bytes: group_count
                    .checked_mul(state_bytes)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(
                    input.rows as f64 * (depth + 1.0),
                    out.bytes,
                    0,
                )),
                format!(
                    "build {} in tumbling windows of {tumbling_window_width_ms} ms for \
                     {group_count} groups: {} rows x (1 + depth {depth})",
                    summary_type_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryMerge { group_by, .. }) => {
            // One operation per input state read; one state per group
            // out: an input's groups are its states of one window (all
            // its states unless it selects a chain's windows). A
            // rollup's groups are those of its keys, at most its input
            // groups.
            let mut group_count = 0;
            for producer in producers {
                group_count = group_count.max(
                    producer
                        .output
                        .rows
                        .div_ceil(producer.tumbling_windows.max(1)),
                );
            }
            let states = merged_states(producers)?;
            if let Some(keys) = group_by {
                group_count = group_count.min(groups(&Reduction::by(keys.clone()))?);
            }
            let out = EdgeStatistics {
                rows: group_count,
                bytes: input
                    .bytes
                    .div_ceil(input.rows.max(1))
                    .checked_mul(group_count)
                    .ok_or(AnalyticalCostError::Overflow)?,
            };
            (
                out,
                Ok(ResourceEstimate::new(states as f64, out.bytes, 0)),
                format!("merge {states} states into {group_count}"),
            )
        }
        Payload::ASAP(ASAPOp::SummaryEstimate { query, .. }) => {
            let per_group = match query {
                SketchStatistic::TopK { k } => *k as u64,
                _ => 1,
            };
            let out = edge(
                input
                    .rows
                    .checked_mul(per_group)
                    .ok_or(AnalyticalCostError::Overflow)?,
            )?;
            (
                out,
                Ok(ResourceEstimate::new(out.rows as f64, 0, 0)),
                format!("estimate {} groups x {per_group}", input.rows),
            )
        }
        Payload::ASAP(ASAPOp::FinalizeExactAccumulator { .. }) => (
            edge(input.rows)?,
            Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
            format!("finalize {} accumulators", input.rows),
        ),
        _ => (
            edge(input.rows)?,
            Ok(ResourceEstimate::new(input.rows as f64, 0, 0)),
            format!("{} rows", input.rows),
        ),
    };
    let estimate = estimate?;
    let (multiplier, per_second) = rate;
    let formula_ms = estimate.cpu_ops() * speed.ms_per_cpu_op
        + estimate.scan_bytes() as f64 * speed.ms_per_scanned_byte;
    // What one computation processes, the unit its entry is priced per
    // ([`OperatorKind`]): the samples a scan reads, the states a merge
    // reads, otherwise the rows of its (first) input.
    let units = match &node.payload {
        Payload::NonASAP(NonASAPOp::Scan { .. }) => out.rows,
        Payload::ASAP(ASAPOp::SummaryMerge { .. }) => merged_states(producers)?,
        _ => input.rows,
    };
    let mut detail = detail;
    // A node with an entry takes its measured time, scanning included;
    // any other node its formulas' CPU operations times the calibration
    // factor, its scanned bytes unscaled (scaling only the CPU keeps it
    // non-negative whatever the factor).
    let estimate = match unit_price {
        Some(unit) => {
            cost_source = NodeCostSource::Measured;
            let measured_ms = (units as f64 * unit.ns_per_unit + unit.ns_per_invocation) * 1e-6;
            fit.record(measured_ms * per_second, formula_ms * per_second);
            detail += &format!(
                "; measured {units} x {:.3} ns + {:.3} ns",
                unit.ns_per_unit, unit.ns_per_invocation
            );
            ResourceEstimate::new(
                measured_ms / speed.ms_per_cpu_op,
                estimate.peak_memory_bytes(),
                0,
            )
        }
        None if formula_scale != 1.0 => {
            detail += &format!("; formula x {formula_scale:.3} (calibration)");
            ResourceEstimate::new(
                estimate.cpu_ops() * formula_scale,
                estimate.peak_memory_bytes(),
                estimate.scan_bytes(),
            )
        }
        None => estimate,
    };
    let node_ms = estimate.cpu_ops() * speed.ms_per_cpu_op
        + estimate.scan_bytes() as f64 * speed.ms_per_scanned_byte;

    // What this node keeps: memory averaged over time (byte-seconds per
    // second), its peak memory and its disk bytes, added to the
    // candidate's usage below.
    let mut average_memory_bytes = 0.0;
    let mut peak_memory_bytes = 0u64;
    let mut disk_bytes = 0u64;
    let mut detail = format!("{detail}; x {per_second:.4}/s");
    // State retained between computations: at ingestion time, the window
    // being built and the completed ones queries read (a chain of N
    // tumbling windows keeps N + 1), every second. At query time the output is
    // held only while the node computes (below). The state is what the
    // node holds: its output, or the values an exact aggregate keeps per
    // group when that is more. A computation that keeps nothing holds its
    // working memory while it runs.
    let state = out.bytes.max(estimate.peak_memory_bytes());
    let (materialized_bytes, medium) = match (ingestion, node.lifetime, &node.payload) {
        (_, Lifetime::None, _) => {
            let working_memory_bytes = estimate.peak_memory_bytes();
            peak_memory_bytes = working_memory_bytes.saturating_mul(multiplier);
            if !ingestion && working_memory_bytes > 0 {
                held_bytes = Some(working_memory_bytes);
            }
            (0, StorageMedium::Memory)
        }
        // A tumbling-window chain materialized across evaluations holds the
        // tumbling windows its retention covers, all but the newest between
        // evaluations and all while one runs. `state` is one window's.
        (
            false,
            Lifetime::AcrossEvaluations(medium),
            Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => {
            let retained = retained_tumbling_window_count;
            let between = state
                .checked_mul(retained - 1)
                .ok_or(AnalyticalCostError::Overflow)?;
            match medium {
                StorageMedium::Memory => {
                    average_memory_bytes = between as f64;
                    peak_memory_bytes = state
                        .checked_mul(retained)
                        .ok_or(AnalyticalCostError::Overflow)?;
                }
                StorageMedium::Disk => disk_bytes = between,
            }
            detail += &format!("; materializes {between} bytes across evaluations");
            (0, medium)
        }
        (_, Lifetime::OneEvaluation(medium), _) => {
            match medium {
                StorageMedium::Memory => {
                    held_bytes = Some(state);
                    peak_memory_bytes = state;
                }
                StorageMedium::Disk => disk_bytes = state,
            }
            (0, medium)
        }
        // The retained tumbling windows and the one being built.
        (
            true,
            Lifetime::AcrossEvaluations(medium),
            Payload::ASAP(ASAPOp::SummaryTumblingWindowChain { .. }),
        ) => (
            state
                .checked_mul(
                    retained_tumbling_window_count
                        .checked_add(1)
                        .ok_or(AnalyticalCostError::Overflow)?,
                )
                .ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
        // Raw rows materialized at ingestion time: the rows it retains.
        (_, Lifetime::AcrossEvaluations(medium), _) if raw_rows_materialized_at_ingestion => {
            (edge(raw_retained_rows.unwrap_or(out.rows))?.bytes, medium)
        }
        // A whole state materialized across evaluations: it and the one being built.
        (_, Lifetime::AcrossEvaluations(medium), _) => (
            state.checked_mul(2).ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
    };
    if ingestion && node.lifetime == Lifetime::AcrossEvaluations(StorageMedium::Memory) {
        maintained = Some(state);
    }
    if materialized_bytes > 0 {
        match medium {
            StorageMedium::Memory => {
                average_memory_bytes = materialized_bytes as f64;
                peak_memory_bytes = materialized_bytes;
            }
            StorageMedium::Disk => disk_bytes = materialized_bytes,
        }
        detail += &format!("; keeps {materialized_bytes} bytes");
    }
    if let Payload::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload {
        if !ingestion {
            let retained = match raw_retained_rows {
                Some(rows) => edge(rows)?.bytes,
                None => out.bytes,
            };
            raw_scan = Some((source, retained));
        }
    }
    // Query-time memory is held only while the node computes (concepts doc,
    // section 5, "Memory held while computing"): its bytes for its work per
    // execution, at its executions per second. Query latency does not
    // enter it.
    if let Some(bytes) = held_bytes {
        average_memory_bytes += bytes as f64 * node_ms / 1_000.0 * per_second;
        detail += &format!("; holds {bytes} bytes for {node_ms:.3} ms");
    }
    // One computation's CPU and scan price times its rate, then the
    // materialized state's.
    let cost = usage_prices.map(|prices| {
        (estimate.cpu_ops() * prices.cost_per_cpu_op
            + estimate.scan_bytes() as f64 * prices.cost_per_scanned_byte)
            * per_second
            + prices.cost_per_memory_byte_second * average_memory_bytes
            + prices.cost_per_disk_byte_second * disk_bytes as f64
    });
    if cost.is_some_and(|cost| !cost.is_finite()) {
        return Err(AnalyticalCostError::Overflow);
    }
    let tumbling_windows = match &node.payload {
        Payload::NonASAP(NonASAPOp::TimeRange { range, .. })
            if chain_tumbling_window_width_ms(producers).is_some() =>
        {
            selected_tumbling_windows(range, producers)?
        }
        Payload::NonASAP(NonASAPOp::TimeShift { .. }) => producers
            .first()
            .map_or(1, |producer| producer.tumbling_windows),
        _ => 1,
    };
    let chain_tumbling_window_width_ms = match &node.payload {
        Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            tumbling_window_width_ms,
            ..
        }) => Some(*tumbling_window_width_ms),
        Payload::NonASAP(NonASAPOp::TimeShift { .. }) => chain_tumbling_window_width_ms(producers),
        _ => None,
    };
    Ok(NodeResourceUsageAndCost {
        output: out,
        sources,
        family,
        unchanged_columns,
        tumbling_windows,
        chain_tumbling_window_width_ms,
        work_ms_per_execution: node_ms,
        cpu_ops_per_second: estimate.cpu_ops() * per_second,
        scanned_bytes_per_second: estimate.scan_bytes() as f64 * per_second,
        average_memory_bytes,
        peak_memory_bytes,
        disk_bytes,
        maintained,
        raw_scan,
        fit,
        cost: NodeCost {
            operator,
            timing: node.output_state.timing,
            cost_per_second: cost,
            cpu_core_seconds_per_second: node_ms * per_second / 1_000.0,
            multiplier,
            detail,
            cost_source,
        },
    })
}

/// [`PlanCost::source`] for nodes priced as `per_node` says.
fn cost_source(per_node: &BTreeMap<PhysicalASAPNodeId, NodeCost>) -> &'static str {
    let measured = |node: &NodeCost| node.cost_source == NodeCostSource::Measured;
    if !per_node.values().any(measured) {
        COST_SOURCE
    } else if per_node.values().all(measured) {
        MEASURED_COST_SOURCE
    } else {
        MIXED_COST_SOURCE
    }
}

/// The CPU cores a candidate needs at its peak: enough for its
/// ingestion-time work every second (none without any), plus one per
/// query with query-time work, all evaluating at once; at least the cores
/// its average query-time work fills.
fn peak_cpu_cores(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
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
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
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

/// The tumbling-window width of the chain the first of `producers` reads
/// from, if any ([`ProducerOutput::chain_tumbling_window_width_ms`]): a `TimeRange` over
/// it selects windows.
fn chain_tumbling_window_width_ms(producers: &[ProducerOutput<'_>]) -> Option<u64> {
    producers.first()?.chain_tumbling_window_width_ms
}

/// The windows a `TimeRange` of `range` selects from the chain
/// `producers` starts with: those inside the range, at least one.
fn selected_tumbling_windows(
    range: &std::time::Duration,
    producers: &[ProducerOutput<'_>],
) -> Result<u64, AnalyticalCostError> {
    let width_ms = chain_tumbling_window_width_ms(producers)
        .unwrap_or(1)
        .max(1);
    Ok(u64::try_from(range.as_millis() / u128::from(width_ms))
        .map_err(|_| AnalyticalCostError::Overflow)?
        .max(1))
}

/// The input states a `SummaryMerge` over `producers` reads: every row of
/// each.
fn merged_states(producers: &[ProducerOutput<'_>]) -> Result<u64, AnalyticalCostError> {
    producers.iter().try_fold(0_u64, |total, producer| {
        total
            .checked_add(producer.output.rows)
            .ok_or(AnalyticalCostError::Overflow)
    })
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

/// The nodes of `dag` by id.
fn node_map(dag: &PhysicalASAPDAG) -> NodeMap<'_> {
    dag.nodes.iter().map(|n| (n.id, n)).collect()
}

/// The nodes of a physical DAG by id.
type NodeMap<'a> =
    HashMap<PhysicalASAPNodeId, &'a asap_types::ir::physical_export::PhysicalASAPDAGNode>;

/// A summary type's name in reports: a sketch by its algorithm, e.g.
/// `HydraCms`; an exact accumulator by its kind.
pub fn summary_type_name(summary_type: &FieldDataType) -> String {
    match summary_type {
        FieldDataType::Sketch(algorithm) => format!("{algorithm:?}"),
        FieldDataType::ExactAggregate(kind, _) => format!("exact {kind:?} accumulator"),
        other => format!("{other:?}"),
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use asap_types::ir::physical_export::PhysicalASAPDAGNode;
    use asap_types::ir::properties::ExecutionDataState;
    use asap_types::ir::scalar::ColumnRef;
    use asap_types::ir::schema::{ExactKind, ExactParams, SummaryUpdate};

    use super::*;

    fn node(payload: Payload) -> PhysicalASAPDAGNode {
        PhysicalASAPDAGNode {
            id: 0,
            payload,
            output_state: ExecutionDataState::QUERY_ROWS,
            lifetime: Lifetime::None,
            parameters: None,
            output_schema: Schema::default(),
            guarantee: None,
            retention_ms: None,
            summary_coverage: None,
            selected_physical_candidate: None,
        }
    }

    fn produced<'a>(
        node: &'a PhysicalASAPDAGNode,
        usage: &NodeResourceUsageAndCost<'a>,
    ) -> ProducerOutput<'a> {
        ProducerOutput {
            payload: &node.payload,
            output_schema: &node.output_schema,
            output: usage.output,
            sources: usage.sources.clone(),
            family: usage.family,
            unchanged_columns: usage.unchanged_columns.clone(),
            tumbling_windows: usage.tumbling_windows,
            chain_tumbling_window_width_ms: usage.chain_tumbling_window_width_ms,
        }
    }

    /// A merge of the five 60 s windows a 5m `TimeRange` selects from a
    /// chain of 1000 states per window reads 5000 states and outputs one
    /// state per group, 1000, not one per state read.
    #[test]
    fn a_merge_over_a_chain_s_windows_outputs_one_state_per_group() {
        let settings = CostInputs::default();
        let data = DataWorkload::default();
        let chain = node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 60_000,
        }));
        let chain_output = ProducerOutput {
            payload: &chain.payload,
            output_schema: &chain.output_schema,
            output: EdgeStatistics {
                rows: 1_000,
                bytes: 16_000,
            },
            sources: BTreeSet::new(),
            family: StateFamily::Exact(&ExactKind::Sum),
            unchanged_columns: BTreeSet::new(),
            tumbling_windows: 1,
            chain_tumbling_window_width_ms: Some(60_000),
        };
        let windows = node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: 0,
        }));
        let windows_usage = node_resource_usage_and_cost(
            &windows,
            &[chain_output],
            (1, 1.0),
            None,
            &data,
            &settings,
            1.0,
        )
        .unwrap();
        assert_eq!(windows_usage.output.rows, 5_000);
        assert_eq!(windows_usage.tumbling_windows, 5);
        let merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
            children: vec![0],
            group_by: None,
        }));
        let merge_usage = node_resource_usage_and_cost(
            &merge,
            &[produced(&windows, &windows_usage)],
            (1, 1.0),
            None,
            &data,
            &settings,
            1.0,
        )
        .unwrap();
        assert_eq!(merge_usage.output.rows, 1_000);
        assert_eq!(merge_usage.output.bytes, 16_000);
        assert!(
            merge_usage
                .cost
                .detail
                .starts_with("merge 5000 states into 1000;"),
            "{}",
            merge_usage.cost.detail
        );
    }

    /// o11y q21's shape, `sum(rate(m[5m] offset 1h))` over a chain of 60 s
    /// windows: a `TimeShift` by 1 h between the chain and the 5m
    /// `TimeRange`, or above the `TimeRange`, changes nothing. The range
    /// selects five windows of 1000 states, and the merge over them reads
    /// 5000 states into 1000, as with no shift (it read 1000, one window).
    #[test]
    fn a_time_shift_over_a_chain_or_its_windows_selects_what_the_range_does() {
        use asap_types::ir::operator::operator_properties::TimeShift;
        let settings = CostInputs::default();
        let data = DataWorkload::default();
        let chain = node(Payload::ASAP(ASAPOp::SummaryTumblingWindowChain {
            child: 0,
            summary_type: FieldDataType::ExactAggregate(ExactKind::Sum, ExactParams::Sum),
            input: SummaryUpdate::column(ColumnRef::SampleValue),
            reduction: Reduction::PerEntity,
            filter: None,
            tumbling_window_width_ms: 60_000,
        }));
        let shift = node(Payload::NonASAP(NonASAPOp::TimeShift {
            shift: TimeShift {
                offset_ms: 3_600_000,
                at: None,
            },
            child: 0,
        }));
        let range = node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: 0,
        }));
        let merge = node(Payload::ASAP(ASAPOp::SummaryMerge {
            children: vec![0],
            group_by: None,
        }));
        // The merge's cost detail and output over `path`, priced bottom-up
        // from the chain's one window of 1000 states.
        let merge_over = |path: &[&PhysicalASAPDAGNode]| -> (String, EdgeStatistics) {
            let mut below = ProducerOutput {
                payload: &chain.payload,
                output_schema: &chain.output_schema,
                output: EdgeStatistics {
                    rows: 1_000,
                    bytes: 16_000,
                },
                sources: BTreeSet::new(),
                family: StateFamily::Exact(&ExactKind::Sum),
                unchanged_columns: BTreeSet::new(),
                tumbling_windows: 1,
                chain_tumbling_window_width_ms: Some(60_000),
            };
            for &step in path {
                let priced = node_resource_usage_and_cost(
                    step,
                    &[below],
                    (1, 1.0),
                    None,
                    &data,
                    &settings,
                    1.0,
                )
                .unwrap();
                below = produced(step, &priced);
            }
            let priced = node_resource_usage_and_cost(
                &merge,
                &[below],
                (1, 1.0),
                None,
                &data,
                &settings,
                1.0,
            )
            .unwrap();
            (priced.cost.detail, priced.output)
        };
        let direct = merge_over(&[&range]);
        assert!(
            direct.0.starts_with("merge 5000 states into 1000;"),
            "{}",
            direct.0
        );
        for path in [[&shift, &range], [&range, &shift]] {
            assert_eq!(merge_over(&path), direct);
        }
    }

    /// Decided 2026-11-09 (A3): a query-time node holds its state only
    /// while it computes: bytes × its work per execution × its executions
    /// per second, with no query latency. A KLL state of 2 MB whose own
    /// work takes 3 ms, run 0.1 times per second, holds 2,000,000 × 0.003 ×
    /// 0.1 = 600 byte·s per second; here, a `OneEvaluation` merge run 0.1
    /// times per second.
    #[test]
    fn query_time_memory_is_held_only_while_the_node_computes() {
        let settings = CostInputs::default();
        let data = DataWorkload::default();
        let input = node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(300),
            kind: TimeRangeKind::Range,
            child: 0,
        }));
        let states = ProducerOutput {
            payload: &input.payload,
            output_schema: &input.output_schema,
            output: EdgeStatistics {
                rows: 5_000,
                bytes: 80_000,
            },
            sources: BTreeSet::new(),
            family: StateFamily::Exact(&ExactKind::Sum),
            unchanged_columns: BTreeSet::new(),
            tumbling_windows: 1,
            chain_tumbling_window_width_ms: None,
        };
        let merge = PhysicalASAPDAGNode {
            lifetime: Lifetime::OneEvaluation(StorageMedium::Memory),
            ..node(Payload::ASAP(ASAPOp::SummaryMerge {
                children: vec![0],
                group_by: None,
            }))
        };
        let usage =
            node_resource_usage_and_cost(&merge, &[states], (1, 0.1), None, &data, &settings, 1.0)
                .unwrap();
        let held: u64 = usage
            .cost
            .detail
            .split("holds ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|bytes| bytes.parse().ok())
            .expect("the held bytes are in the detail");
        assert!(held > 0);
        let expected = held as f64 * usage.work_ms_per_execution / 1_000.0 * 0.1;
        assert!(
            (usage.average_memory_bytes - expected).abs() <= 1e-9 * expected,
            "{} vs {expected}",
            usage.average_memory_bytes
        );
    }

    /// Decided 2026-11-09 (A5): a query-time `Scan` keeps what its
    /// `retention_ms` covers but reads, per execution, only the most its
    /// consumers read (no range-evaluation span): under a `TimeRange` of 1 s
    /// over 1,000 rows per second it reads 1,000 rows while it keeps
    /// 3,601,000 (a 1 h range query with step 1 s over an instant
    /// selector).
    #[test]
    fn a_query_time_scan_reads_what_its_consumers_read_and_keeps_its_retention() {
        use asap_types::ir::schema::{DataType, Field};
        use asap_types::workload::DurationMs;
        let settings = CostInputs::default();
        let mut data = DataWorkload::sampled_every(["x".to_string()], DurationMs(1_000));
        for facts in data.sources.values_mut() {
            facts.series_count = Some(1_000);
        }
        let scan = PhysicalASAPDAGNode {
            retention_ms: Some(3_601_000),
            output_schema: Schema::new(vec![
                Field::plain("ts", DataType::Timestamp, false),
                Field::plain("value", DataType::Float64, false),
            ]),
            ..node(Payload::NonASAP(NonASAPOp::Scan {
                source: Source::TimeSeries { metric: "x".into() },
                predicates: vec![],
                schema: Schema::default(),
            }))
        };
        let instant = node(Payload::NonASAP(NonASAPOp::TimeRange {
            range: Duration::from_secs(1),
            kind: TimeRangeKind::Range,
            child: 0,
        }));
        let read_ms = read_back_ms([1], |_| &instant.payload, |_| vec![], |_| 0, |_| true, true);
        assert_eq!(read_ms, Some(1_000));
        let usage =
            node_resource_usage_and_cost(&scan, &[], (1, 1.0), read_ms, &data, &settings, 1.0)
                .unwrap();
        assert_eq!(usage.output.rows, 1_000);
        let (_, kept_bytes) = usage.raw_scan.expect("a query-time scan keeps raw data");
        assert_eq!(kept_bytes, usage.output.bytes * 3_601);
    }
}
