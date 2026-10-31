//! Costing a physical plan DAG node by node: each node's resource usage
//! ([`node_usage`]), then the plan's usage, latency, retention, kept pane
//! chains and cost ([`plan_cost`]). Moved from Stage 3 (`asap-plan-selection`,
//! whose module doc, "Cost model", describes the model), so Stage 3 computes
//! no cost of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::analytical::{
    estimate_operator, AnalyticalCostError, PhysicalOperator, ResourceEstimate,
};
use crate::execution_demand::{computations, consumers, evaluation_rate, query_starts, Consumer};
use crate::operator_statistics::{
    EdgeStatistics, OperatorStatistics, PartitionStatistics, UnaryEdgeStatistics,
};
use crate::state_bytes::{
    keeps_every_value, row_bytes, sketch_shape, summary_shape, StateInputRows,
};
use crate::{aggregate_lookup, lookup, StateFamily};
use crate::{
    CalibrationFit, CostInputs, CostModel, NodeCost, NodePriceSource, OperatorKind, PlanCost,
    QueryLatency, ResourceUsage, COST_SOURCE, COST_UNIT, MEASURED_COST_SOURCE, MIXED_COST_SOURCE,
};
use asap_types::ir::operator::{GroupKeys, Reduction, Source};
use asap_types::ir::physical_export::{
    PhysicalASAPDAG, PhysicalASAPNodeId, PhysicalASAPOperatorPayload as Payload, ScanWindow,
};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::schema::{
    FieldDataType, GroupingStrategy, Schema, SketchParams, SketchStatistic,
};
use asap_types::ir::{ASAPOp, NonASAPOp, TimeRangeKind};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryRecurrence, RangeEvaluation};

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
/// rows. A materialized node's kept state is counted in byte-seconds per
/// second; a pane chain kept beyond one evaluation keeps the panes its
/// retention covers ([`pane_retention_ms`]). `nodes` and `kept_chains` are
/// `dag`'s ([`node_map`], [`kept_pane_chains`]); `recurrence[q]` and
/// `range_evaluations[q]` say how often query `q` runs and how many steps
/// each run evaluates (one when missing).
#[allow(clippy::too_many_arguments)]
pub fn plan_cost(
    dag: &PhysicalASAPDAG,
    nodes: &NodeMap<'_>,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
    recurrence: &[QueryRecurrence],
    range_evaluations: &[Option<RangeEvaluation>],
    data: &DataWorkload,
    settings: &CostInputs,
    formula_scale: f64,
) -> Result<(PlanCost, CalibrationFit), (PhysicalASAPNodeId, AnalyticalCostError)> {
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
        reach,
    } = each_node_usage(
        dag,
        nodes,
        kept_chains,
        recurrence,
        range_evaluations,
        data,
        settings,
        formula_scale,
        None,
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
    // Query-time memory held only while one evaluation runs, a MatAtQuery
    // state in memory or a NotMaterialized node's working memory: (bytes,
    // per second, whether each consumer computes its own copy),
    // counted once the evaluations' latencies are known, in node order so
    // the sum is the same in every run.
    let mut held: BTreeMap<PhysicalASAPNodeId, (u64, f64, bool)> = BTreeMap::new();
    // The raw data query-time scans read, kept on disk: per source, the
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
        if let Some(entry) = node_priced.held {
            held.insert(node.id, entry);
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
    // Query-time memory is held for one evaluation: a MatAtQuery state from
    // its build until the longest query reading it finishes, about that
    // query's latency, once per execution. A NotMaterialized node's working
    // memory is computed once per consumer ([`consumers`]), and each copy is
    // held while its consumer's evaluation runs: a query's for that query's
    // latency, at its rate; a materialized node's until the longest query
    // reading that node finishes, at the node's rate, as its MatAtQuery
    // state is.
    let longest_reader_ms = |id: PhysicalASAPNodeId| {
        reach
            .iter()
            .zip(&latency)
            .filter(|(reached, _)| reached.contains(&id))
            .map(|(_, l)| l.total_ms())
            .fold(0.0, f64::max)
    };
    let consumers_of = consumers(dag);
    for (id, (state, per_second, computed_per_consumer)) in held {
        // The milliseconds it is held per second: each execution's held
        // time times its executions per second, summed.
        let held_ms_per_second = match computed_per_consumer {
            false => per_second * longest_reader_ms(id),
            true => {
                let mut sum = 0.0;
                for &consumer in consumers_of.get(&id).into_iter().flatten() {
                    sum += match consumer {
                        Consumer::Query(query) => {
                            let rate = evaluation_rate(
                                recurrence
                                    .get(query)
                                    .map(|r| (r, range_evaluations.get(query).copied().flatten())),
                                settings.non_repeating_query_horizon_seconds,
                            )
                            .map_err(|error| (id, error))?;
                            rate * latency.get(query).map_or(0.0, QueryLatency::total_ms)
                        }
                        Consumer::Materialized(node) => {
                            multipliers[&node].1 * longest_reader_ms(node)
                        }
                    };
                }
                sum
            }
        };
        let evaluation_ms = match per_second > 0.0 {
            true => held_ms_per_second / per_second,
            false => 0.0,
        };
        let byte_seconds = state as f64 * held_ms_per_second / 1_000.0;
        let node = per_node.get_mut(&id).expect("every node is measured");
        usage.average_memory_bytes += byte_seconds;
        node.detail += &format!("; holds {state} bytes for {evaluation_ms:.1} ms");
        if let (Some(cost), Some(prices)) = (&mut node.cost_per_second, usage_prices) {
            *cost += prices.cost_per_memory_byte_second * byte_seconds;
            if !cost.is_finite() {
                return Err((id, AnalyticalCostError::Overflow));
            }
        }
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
            cost_unit: COST_UNIT,
            source: if has_unattached_request_overhead {
                if per_node
                    .values()
                    .all(|node| node.price_source == NodePriceSource::Measured)
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
/// node's `(multiplier, per_second)` ([`computations`]) and the nodes each
/// query reaches at query time ([`query_reach`]).
struct EachNodeUsage<'a> {
    priced: HashMap<PhysicalASAPNodeId, NodeUsage<'a>>,
    multipliers: HashMap<PhysicalASAPNodeId, (u64, f64)>,
    reach: Vec<HashSet<PhysicalASAPNodeId>>,
}

/// [`node_usage`] over every node of `dag`, children first, with the
/// arguments of [`plan_cost`]. `root_scan_lookback_ms` is the
/// look-back of a query-time scan no window above it in `dag` bounds (a
/// scan priced without its consumers); `None` when the scan has no
/// window, so it reads the source's `rows_held` or fails with
/// `MissingSourceFact`, as Stage 3 does. `root_rates`, when given, are
/// the computations per second of each root's consumer in place of its
/// query's own rate ([`computations`]).
#[allow(clippy::too_many_arguments)]
fn each_node_usage<'a>(
    dag: &'a PhysicalASAPDAG,
    nodes: &NodeMap<'a>,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
    recurrence: &[QueryRecurrence],
    range_evaluations: &[Option<RangeEvaluation>],
    data: &DataWorkload,
    settings: &'a CostInputs,
    formula_scale: f64,
    root_scan_lookback_ms: Option<u64>,
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
    let reach = query_reach(dag, nodes);
    let retention_ms = pane_retention_ms(dag, &reach, range_evaluations);
    let mut producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for e in &dag.edges {
        producers_of.entry(e.consumer).or_default().push(e.producer);
    }
    let mut priced: HashMap<PhysicalASAPNodeId, NodeUsage<'a>> = HashMap::new();
    for node in &dag.nodes {
        let views: Vec<ProducerOutput<'a>> = producers_of
            .get(&node.id)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|p| {
                let produced: &NodeUsage<'a> = &priced[p];
                ProducerOutput {
                    payload: &nodes[p].payload,
                    output_schema: &nodes[p].output_schema,
                    output: produced.output,
                    sources: produced.sources.clone(),
                    family: produced.family,
                    unchanged_columns: produced.unchanged_columns.clone(),
                }
            })
            .collect();
        let scan_window = match (&node.payload, node.output_state.timing) {
            (Payload::NonASAP(NonASAPOp::Scan { .. }), ExecutionTiming::QueryTime) => {
                match dag.scan_window(node.id, kept_chains) {
                    // Priced as before #955 decides `@` anchors and
                    // unwindowed readers: the widest window read.
                    ScanWindow::Unbounded => trailing_window(
                        widest_window_read_ms(dag, nodes, node.id, kept_chains)
                            .or(root_scan_lookback_ms),
                    ),
                    window => window,
                }
            }
            _ => ScanWindow::Unbounded,
        };
        let node_priced = node_usage(
            node,
            &views,
            multipliers[&node.id],
            scan_window,
            retained_panes(node, &retention_ms),
            (node.output_state.timing == ExecutionTiming::IngestionTime)
                .then(|| maintained_window_ms(node.id, nodes, &producers_of))
                .flatten(),
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
        reach,
    })
}

/// The state family of a node with `payload` whose first producer has
/// `producer_family`: a build's own (one sketch instance per group; a
/// shared Hydra structure is not that sketch), inherited by the merges,
/// estimates and finalizations over it, none otherwise.
fn state_family<'a>(
    payload: &'a Payload,
    producer_family: Option<StateFamily<'a>>,
) -> StateFamily<'a> {
    match payload {
        Payload::ASAP(
            ASAPOp::SummaryAgg {
                family, grouping, ..
            }
            | ASAPOp::PaneChain {
                family, grouping, ..
            },
        ) => match family {
            FieldDataType::Sketch(kind, _)
                if *grouping == GroupingStrategy::PerSubpopulationInstance =>
            {
                StateFamily::Sketch(kind.params())
            }
            FieldDataType::ExactAggregate(kind, _) => StateFamily::Exact(kind),
            _ => StateFamily::None,
        },
        Payload::ASAP(
            ASAPOp::SummaryMerge { .. }
            | ASAPOp::SummaryEstimate { .. }
            | ASAPOp::FinalizeExactAccumulator { .. },
        ) => producer_family.unwrap_or(StateFamily::None),
        _ => StateFamily::None,
    }
}

/// One producer of a node being priced ([`node_usage`]): its payload and
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
}

/// What one node uses and outputs ([`node_usage`]): its output statistics, sources and state family for its
/// consumers; its work for one computation (one execution) in ms on one
/// core; its usage per second; what is charged
/// once the whole plan is known (memory held for a query's evaluation, an
/// in-memory ingestion window's partial copies, the raw data a query-time
/// scan keeps on disk); its calibration fit and its [`NodeCost`].
#[derive(Debug, Clone)]
pub struct NodeUsage<'a> {
    pub output: EdgeStatistics,
    pub sources: BTreeSet<&'a str>,
    pub family: StateFamily<'a>,
    /// Its output columns that still hold one source row's label (or
    /// column) values unchanged ([`unchanged_source_columns`]).
    pub unchanged_columns: BTreeSet<&'a str>,
    pub work_ms_per_execution: f64,
    pub cpu_ops_per_second: f64,
    pub scanned_bytes_per_second: f64,
    pub average_memory_bytes: f64,
    pub peak_memory_bytes: u64,
    pub disk_bytes: u64,
    /// Query-time memory held while an evaluation runs: (bytes, computations
    /// per second, whether each consumer computes its own copy, held while
    /// that consumer's evaluation runs ([`plan_cost`]), rather than one copy
    /// held until the longest query reading the node finishes).
    pub held: Option<(u64, f64, bool)>,
    /// An in-memory ingestion-time state's bytes per window.
    pub maintained: Option<u64>,
    /// A query-time scan's source and the raw bytes it reads.
    pub raw_scan: Option<(&'a Source, u64)>,
    pub fit: CalibrationFit,
    pub cost: NodeCost,
}

/// Measure and price one node of a physical DAG (one step of
/// [`plan_cost`]): `producers` are its producers in edge order,
/// `rate` its `(multiplier, per_second)` ([`computations`]), `scan_window`
/// the raw rows a query-time scan reads ([`PhysicalASAPDAG::scan_window`];
/// read only for scans: it reads `width_ms` of rows and keeps them back to
/// [`ScanWindow::lookback_ms`]), `retained_pane_count` the panes a pane chain keeps
/// completed (`retained_panes`), and `ingestion_window_ms` the window an
/// ingestion-time node maintains (`maintained_window_ms`).
#[allow(clippy::too_many_arguments)]
pub fn node_usage<'a>(
    node: &'a asap_types::ir::physical_export::PhysicalASAPDAGNode,
    producers: &[ProducerOutput<'a>],
    rate: (u64, f64),
    scan_window: ScanWindow,
    retained_pane_count: u64,
    ingestion_window_ms: Option<u64>,
    data: &DataWorkload,
    settings: &'a CostInputs,
    formula_scale: f64,
) -> Result<NodeUsage<'a>, AnalyticalCostError> {
    let table = &settings.measured_operation_costs;
    let speed = settings.resource_speed;
    let usage_prices = match &settings.cost_model {
        CostModel::UsageOverTime(prices) => Some(prices),
        _ => None,
    };
    let sketch_size =
        |params: &SketchParams| sketch_shape(params, StateInputRows::UNKNOWN).1 as f64;
    let mut fit = CalibrationFit::default();
    let mut held = None;
    let mut maintained = None;
    let mut raw_scan = None;
    // A query-time scan's raw rows kept ([`ScanWindow::lookback_ms`]).
    let mut raw_kept_rows = None;
    let ingestion = node.output_state.timing == ExecutionTiming::IngestionTime;
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
    // The fact a rate needs, named when it is missing.
    let missing_rate = |source: &str| AnalyticalCostError::MissingSourceFact {
        source_name: source.to_string(),
        missing: "an arrival rate (the arrival's rows_per_second, or the \
                  source's series_count with sample_interval)",
    };
    let family = state_family(&node.payload, producers.first().map(|p| p.family));
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
    let mut price_source = NodePriceSource::Analytical;
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
            let window_ms =
                ingestion_window_ms.ok_or(AnalyticalCostError::IngestionGroupingWithoutWindow)?;
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
                let window_width_ms = match scan_window {
                    ScanWindow::Within { width_ms, .. } => {
                        Some(u64::try_from(width_ms).unwrap_or(0))
                    }
                    ScanWindow::Unbounded => None,
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
                // The raw rows kept for a query-time scan: back to the
                // oldest row it reads, `end_offset + width` (an offset
                // window keeps more than it reads), at most those held.
                if !ingestion {
                    let kept_rows = match (by_rate(scan_window.lookback_ms()), rows_held) {
                        (Some(by_rate), Some(rows_held)) => Some(by_rate.min(rows_held)),
                        (by_rate, rows_held) => by_rate.or(rows_held),
                    };
                    raw_kept_rows = kept_rows.map(|rows| rows.max(1));
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
            NonASAPOp::TimeShift { .. } => (
                input,
                estimate_operator(
                    PhysicalOperator::TimeShift,
                    OperatorStatistics::PassThrough {
                        edges: unary(input),
                    },
                ),
                format!("time shift: pass {} rows, free", input.rows),
            ),
            NonASAPOp::TimeRange { range, kind, .. } => {
                // A range keeps its window's samples; an instant
                // selector the latest sample of each series.
                // Unknown rate or series: no fewer than its input.
                let kept = match kind {
                    TimeRangeKind::Range => rows_per_ms.map_or(input.rows, |rows_per_ms| {
                        (rows_per_ms * range.as_millis() as f64).round() as u64
                    }),
                    TimeRangeKind::Instant => series.unwrap_or(input.rows),
                };
                let out = edge(input.rows.min(kept.max(1)))?;
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
            family, reduction, ..
        }) => {
            let group_count = groups(reduction)?;
            // At query time, each state sees its group's input rows; at
            // ingestion time, the rows of a window one second of
            // arriving rows does not bound.
            let input_rows = StateInputRows {
                rows_per_state: (!ingestion).then(|| input.rows.div_ceil(group_count.max(1))),
                non_negative: raw_data_samples_non_negative(),
            };
            let (depth, state_bytes) = summary_shape(family, input_rows);
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
                    family_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::PaneChain {
            family,
            reduction,
            pane_width_ms,
            pane_count,
            ..
        }) => {
            // Each input row is kept by the one pane its time falls in,
            // as a time range keeps it (one operation), and updates
            // that pane.
            let group_count = groups(reduction)?;
            let states = group_count
                .checked_mul(u64::from(*pane_count))
                .ok_or(AnalyticalCostError::Overflow)?;
            // Each pane's state sees its group's rows in one pane: of
            // one second's rows at ingestion time, of the rows read at
            // query time (all of them for a chain kept across
            // evaluations, whose scan Stage 2 bounds to its newest pane,
            // the only one it builds).
            let rows_per_pane = match (ingestion, node.materialization) {
                (true, _) => u64::try_from(
                    (u128::from(input.rows) * u128::from(*pane_width_ms)).div_ceil(1_000),
                )
                .map_err(|_| AnalyticalCostError::Overflow)?,
                (false, Materialization::MatAtQueryKeptAcrossEvaluations(_)) => input.rows,
                (false, _) => input.rows.div_ceil(u64::from(*pane_count).max(1)),
            };
            let input_rows = StateInputRows {
                rows_per_state: Some(rows_per_pane.div_ceil(group_count.max(1))),
                non_negative: raw_data_samples_non_negative(),
            };
            let (depth, state_bytes) = summary_shape(family, input_rows);
            let state_bytes = measured_state_bytes.unwrap_or(state_bytes);
            let out = EdgeStatistics {
                rows: states,
                bytes: states
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
                    "build {} in {pane_count} panes of {pane_width_ms} ms for \
                     {group_count} groups: {} rows x (1 + depth {depth})",
                    family_name(family),
                    input.rows
                ),
            )
        }
        Payload::ASAP(ASAPOp::SummaryMerge {
            newest_panes,
            group_by,
            ..
        }) => {
            // One operation per input state read; one state per group
            // out, each pane of a chain being a state of its group. A
            // merge of a chain's newest `n` panes reads `n` per group. A
            // rollup's groups are those of its keys, at most its input
            // groups.
            let mut group_count = 0;
            for producer in producers {
                let panes = match producer.payload {
                    Payload::ASAP(ASAPOp::PaneChain { pane_count, .. }) => u64::from(*pane_count),
                    _ => 1,
                };
                group_count = group_count.max(producer.output.rows / panes.max(1));
            }
            let states = merged_states(*newest_panes, producers)?;
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
        Payload::ASAP(ASAPOp::SummaryMerge { newest_panes, .. }) => {
            merged_states(*newest_panes, producers)?
        }
        _ => input.rows,
    };
    let mut detail = detail;
    // A node with an entry takes its measured time, scanning included;
    // any other node its formulas' CPU operations times the calibration
    // factor, its scanned bytes unscaled (scaling only the CPU keeps it
    // non-negative whatever the factor).
    let estimate = match unit_price {
        Some(unit) => {
            price_source = NodePriceSource::Measured;
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
    // State kept between computations: at ingestion time, the window
    // being built and the completed ones queries read (a chain of N
    // panes keeps N + 1), every second. At query time the output is
    // reused only within one evaluation, so it is held only while that
    // evaluation runs (counted below, once latencies are known). The
    // state is what the node holds: its output, or the values an exact
    // aggregate keeps per group when that is more. A computation that
    // keeps nothing holds its working memory while it runs, once per
    // consumer that recomputes it; at query time, that is held for the
    // evaluation too (counted in [`plan_cost`], per consumer: a query's
    // latency, or a materialized consumer's longest reader's, as a
    // MatAtQuery state is).
    let state = out.bytes.max(estimate.peak_memory_bytes());
    let (kept, medium) = match (node.materialization, &node.payload) {
        (Materialization::NotMaterialized, _) => {
            let working_memory_bytes = estimate.peak_memory_bytes();
            peak_memory_bytes = working_memory_bytes.saturating_mul(multiplier);
            if !ingestion && working_memory_bytes > 0 {
                held = Some((working_memory_bytes, per_second, true));
            }
            (0, Medium::Memory)
        }
        // A pane chain kept across evaluations holds the panes its
        // retention covers ([`pane_retention_ms`]: N, plus one per pane
        // width of the readers' range-evaluation span), all but the
        // newest between evaluations and all while one runs.
        (Materialization::MatAtQueryKeptAcrossEvaluations(medium), _) => {
            let panes = panes_of(node);
            let retained = retained_pane_count;
            let between = (state / panes)
                .checked_mul(
                    retained
                        .checked_sub(1)
                        .ok_or(AnalyticalCostError::Overflow)?,
                )
                .ok_or(AnalyticalCostError::Overflow)?;
            match medium {
                Medium::Memory => {
                    average_memory_bytes = between as f64;
                    peak_memory_bytes = match retained == panes {
                        true => state,
                        false => (state / panes)
                            .checked_mul(retained)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    };
                }
                Medium::Disk => disk_bytes = between,
            }
            detail += &format!("; keeps {between} bytes across evaluations");
            (0, medium)
        }
        (Materialization::MatAtQuery(medium), _) => {
            match medium {
                Medium::Memory => {
                    held = Some((state, per_second, false));
                    peak_memory_bytes = state;
                }
                Medium::Disk => disk_bytes = state,
            }
            (0, medium)
        }
        // The retained panes and the one being built.
        (Materialization::MatAtIngestion(medium), Payload::ASAP(ASAPOp::PaneChain { .. })) => {
            let panes = panes_of(node);
            (
                (state / panes)
                    .checked_mul(
                        retained_pane_count
                            .checked_add(1)
                            .ok_or(AnalyticalCostError::Overflow)?,
                    )
                    .ok_or(AnalyticalCostError::Overflow)?,
                medium,
            )
        }
        (Materialization::MatAtIngestion(medium), _) => (
            state.checked_mul(2).ok_or(AnalyticalCostError::Overflow)?,
            medium,
        ),
    };
    if node.materialization == Materialization::MatAtIngestion(Medium::Memory) {
        maintained = Some(state / panes_of(node));
    }
    if kept > 0 {
        match medium {
            Medium::Memory => {
                average_memory_bytes = kept as f64;
                peak_memory_bytes = kept;
            }
            Medium::Disk => disk_bytes = kept,
        }
        detail += &format!("; keeps {kept} bytes");
    }
    if let Payload::NonASAP(NonASAPOp::Scan { source, .. }) = &node.payload {
        if !ingestion {
            let kept = match raw_kept_rows {
                Some(rows) => edge(rows)?.bytes,
                None => out.bytes,
            };
            raw_scan = Some((source, kept));
        }
    }
    // One computation's CPU and scan price times its rate, then the
    // kept state's.
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
    Ok(NodeUsage {
        output: out,
        sources,
        family,
        unchanged_columns,
        work_ms_per_execution: node_ms,
        cpu_ops_per_second: estimate.cpu_ops() * per_second,
        scanned_bytes_per_second: estimate.scan_bytes() as f64 * per_second,
        average_memory_bytes,
        peak_memory_bytes,
        disk_bytes,
        held,
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
            price_source,
        },
    })
}

/// [`PlanCost::source`] for nodes priced as `per_node` says.
fn cost_source(per_node: &BTreeMap<PhysicalASAPNodeId, NodeCost>) -> &'static str {
    let measured = |node: &NodeCost| node.price_source == NodePriceSource::Measured;
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
        let states = (output[&state].rows / panes_of(nodes[&state])) as f64;
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

/// For each tumbling-pane chain of `dag` kept beyond one evaluation (at
/// ingestion time, or at query time across evaluations), how far back
/// from the newest evaluation its completed panes must be kept, in
/// milliseconds: how long before the evaluation its newest pane ends (the
/// offset of its input, `chain_input_offset_ms`), plus its window
/// `pane_count · pane_width_ms`, plus the longest
/// `span_ms` ([`RangeEvaluation`]) among the queries reaching it,
/// whose earliest step reads that much further back. `reach` is
/// [`query_reach`] of `dag`; `range_evaluations[q]` is query `q`'s (none
/// when missing). Without range evaluations, each chain's window.
pub fn pane_retention_ms(
    dag: &PhysicalASAPDAG,
    reach: &[HashSet<PhysicalASAPNodeId>],
    range_evaluations: &[Option<RangeEvaluation>],
) -> BTreeMap<PhysicalASAPNodeId, u64> {
    let nodes = node_map(dag);
    dag.nodes
        .iter()
        .filter_map(|node| match (&node.payload, node.materialization) {
            (
                Payload::ASAP(ASAPOp::PaneChain {
                    child,
                    pane_width_ms,
                    pane_count,
                    ..
                }),
                Materialization::MatAtIngestion(_)
                | Materialization::MatAtQueryKeptAcrossEvaluations(_),
            ) => {
                let span_ms = reach
                    .iter()
                    .enumerate()
                    .filter(|(_, reached)| reached.contains(&node.id))
                    .filter_map(|(q, _)| range_evaluations.get(q).copied().flatten())
                    .map(|range| range.span_ms)
                    .max()
                    .unwrap_or(0);
                let end_offset_ms = chain_input_offset_ms(*child, &nodes);
                Some((
                    node.id,
                    end_offset_ms + pane_width_ms * u64::from(*pane_count) + span_ms,
                ))
            }
            _ => None,
        })
        .collect()
}

/// How long before the evaluation a chain's newest pane ends: the offsets
/// of the `TimeShift`s (without an `@` anchor) directly below the chain,
/// added up; 0 for a negative total.
fn chain_input_offset_ms(
    input: PhysicalASAPNodeId,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
) -> u64 {
    let mut offset_ms: i64 = 0;
    let mut current = input;
    while let Some(Payload::NonASAP(NonASAPOp::TimeShift { shift, child })) =
        nodes.get(&current).map(|node| &node.payload)
    {
        if shift.at.is_some() {
            break;
        }
        offset_ms = offset_ms.saturating_add(shift.offset_ms);
        current = *child;
    }
    u64::try_from(offset_ms).unwrap_or(0)
}

/// The pane retention of every kept pane chain of `dag`
/// ([`pane_retention_ms`] over [`query_reach`]), for a plan whose query
/// `q` has range evaluation `range_evaluations[q]` (none when missing):
/// what Stage 2's realizer records for a realized plan.
pub fn pane_retention_ms_of_plan(
    dag: &PhysicalASAPDAG,
    range_evaluations: &[Option<RangeEvaluation>],
) -> BTreeMap<PhysicalASAPNodeId, u64> {
    pane_retention_ms(dag, &query_reach(dag, &node_map(dag)), range_evaluations)
}

/// The input states a `SummaryMerge` over `producers` reads: every row of
/// each, or of the one pane chain whose newest `newest_panes` it merges,
/// that many panes' states per group.
fn merged_states(
    newest_panes: Option<u32>,
    producers: &[ProducerOutput<'_>],
) -> Result<u64, AnalyticalCostError> {
    producers.iter().try_fold(0_u64, |total, producer| {
        let rows = match (newest_panes, producer.payload) {
            (Some(count), Payload::ASAP(ASAPOp::PaneChain { pane_count, .. })) => {
                (producer.output.rows / u64::from(*pane_count).max(1))
                    .checked_mul(u64::from(count))
                    .ok_or(AnalyticalCostError::Overflow)?
            }
            _ => producer.output.rows,
        };
        total.checked_add(rows).ok_or(AnalyticalCostError::Overflow)
    })
}

/// The panes a chain in `retention_ms` ([`pane_retention_ms`]) keeps
/// completed: its retention over its pane width, at least its pane count.
pub fn retained_panes(
    node: &asap_types::ir::physical_export::PhysicalASAPDAGNode,
    retention_ms: &BTreeMap<PhysicalASAPNodeId, u64>,
) -> u64 {
    match (&node.payload, retention_ms.get(&node.id)) {
        (Payload::ASAP(ASAPOp::PaneChain { pane_width_ms, .. }), Some(&retention)) => {
            (retention / pane_width_ms.max(&1)).max(panes_of(node))
        }
        _ => panes_of(node),
    }
}

/// A node's panes: a pane chain's count, 1 for any other node.
pub fn panes_of(node: &asap_types::ir::physical_export::PhysicalASAPDAGNode) -> u64 {
    match &node.payload {
        Payload::ASAP(ASAPOp::PaneChain { pane_count, .. }) => u64::from(*pane_count).max(1),
        _ => 1,
    }
}

/// The nodes each query reaches at query time, in workload order: its walk
/// from its roots, stopping at (and including) ingestion-time nodes.
pub fn query_reach(
    dag: &PhysicalASAPDAG,
    nodes: &HashMap<PhysicalASAPNodeId, &asap_types::ir::physical_export::PhysicalASAPDAGNode>,
) -> Vec<HashSet<PhysicalASAPNodeId>> {
    query_starts(dag)
        .into_iter()
        .map(|mut stack| {
            let mut seen = HashSet::new();
            while let Some(id) = stack.pop() {
                if seen.insert(id)
                    && nodes[&id].output_state.timing != ExecutionTiming::IngestionTime
                {
                    stack.extend(
                        dag.edges
                            .iter()
                            .filter(|e| e.consumer == id)
                            .map(|e| e.producer),
                    );
                }
            }
            seen
        })
        .collect()
}

/// The window an ingestion-time grouping `node` maintains: a pane chain's
/// panes, or the time range below it (through filters, projections and
/// time shifts). `None` when there is none.
pub fn maintained_window_ms(
    node: PhysicalASAPNodeId,
    nodes: &NodeMap<'_>,
    producers_of: &HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>>,
) -> Option<u64> {
    match &nodes[&node].payload {
        Payload::ASAP(ASAPOp::PaneChain {
            pane_width_ms,
            pane_count,
            ..
        }) => return Some(pane_width_ms.saturating_mul(u64::from(*pane_count))),
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

/// The window of a scan reading the `lookback_ms` up to the evaluation
/// (`Unbounded` for `None`).
pub fn trailing_window(lookback_ms: Option<u64>) -> ScanWindow {
    lookback_ms.map_or(ScanWindow::Unbounded, |width_ms| ScanWindow::Within {
        end_offset_ms: 0,
        width_ms: i64::try_from(width_ms).unwrap_or(i64::MAX),
    })
}

/// The nodes of `dag` by id.
pub fn node_map(dag: &PhysicalASAPDAG) -> NodeMap<'_> {
    dag.nodes.iter().map(|n| (n.id, n)).collect()
}

/// The nodes of a physical DAG by id.
pub type NodeMap<'a> =
    HashMap<PhysicalASAPNodeId, &'a asap_types::ir::physical_export::PhysicalASAPDAGNode>;

/// The tumbling-pane chains of `dag` kept across evaluations
/// (`MatAtQueryKeptAcrossEvaluations`, which Stage 2 offers only when the
/// deployment keeps query-time results and every reader builds one new
/// pane per evaluation). Each evaluation builds the newest pane, and the
/// older ones, which complete the readers' look-back, are kept from
/// earlier evaluations.
pub fn kept_pane_chains(dag: &PhysicalASAPDAG) -> HashSet<PhysicalASAPNodeId> {
    dag.nodes
        .iter()
        .filter(|node| {
            matches!(
                (&node.payload, node.materialization),
                (
                    Payload::ASAP(ASAPOp::PaneChain { .. }),
                    Materialization::MatAtQueryKeptAcrossEvaluations(_),
                )
            )
        })
        .map(|node| node.id)
        .collect()
}

/// The widest window, in milliseconds, read above the scan `scan` at query
/// time, ignoring readers that read it outside a window and the anchors of
/// shifts: each range `TimeRange` and `PaneChain` (its whole window, or
/// one pane for a chain in `kept_chains`) reached through time shifts.
/// Prices a scan whose [`PhysicalASAPDAG::scan_window`] is unbounded as
/// Stage 3 did before; #955 decides `@` anchors.
fn widest_window_read_ms(
    dag: &PhysicalASAPDAG,
    nodes: &NodeMap<'_>,
    scan: PhysicalASAPNodeId,
    kept_chains: &HashSet<PhysicalASAPNodeId>,
) -> Option<u64> {
    let mut widest = None;
    let mut stack = vec![scan];
    let mut seen = HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
            continue;
        }
        for edge in dag.edges.iter().filter(|e| e.producer == id) {
            let width_ms = match &nodes[&edge.consumer].payload {
                Payload::NonASAP(NonASAPOp::TimeRange { range, .. }) => range.as_millis() as u64,
                Payload::ASAP(ASAPOp::PaneChain { pane_width_ms, .. })
                    if kept_chains.contains(&edge.consumer) =>
                {
                    *pane_width_ms
                }
                Payload::ASAP(ASAPOp::PaneChain {
                    pane_width_ms,
                    pane_count,
                    ..
                }) => pane_width_ms.saturating_mul(u64::from(*pane_count)),
                Payload::NonASAP(NonASAPOp::TimeShift { .. }) => {
                    stack.push(edge.consumer);
                    continue;
                }
                _ => continue,
            };
            widest = widest.max(Some(width_ms));
        }
    }
    widest
}

/// A summary family's name in reports: a shared (Hydra) structure by its
/// Hydra kind, e.g. `HydraCms`; a sketch by its algorithm; an exact
/// accumulator by its kind.
pub fn family_name(family: &FieldDataType) -> String {
    match family {
        FieldDataType::Sketch(_, GroupingStrategy::SharedMultiSubpopulation { kind, .. }) => {
            format!("{kind:?}")
        }
        FieldDataType::Sketch(kind, _) => format!("{:?}", kind.algorithm()),
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
