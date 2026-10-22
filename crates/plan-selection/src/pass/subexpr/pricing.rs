//! Local prices of implementation candidates, cached, and the frozen
//! calibration factor (planner-layering design, "The stages over a memo
//! of sub-expressions"). Cost model 1 only: cost models 2 and 3 keep the current
//! search.
//!
//! A candidate's local price is Stage 3's per-node price
//! ([`crate::price_node`]) of its private nodes, from its node down to, but
//! excluding, its child groups' nodes, computed for the consumers the
//! search fixed top-down ([`ConsumerContext`]) and reading its children's
//! given outputs. Stage 2 builds the candidate's sub-DAG once per
//! materialization; a child's nodes in it are recognized by structure
//! (payload over its producers, and schema) from the child group's own
//! sub-DAG.
//!
//! Each of Stage 3's charges is either per node, priced exactly here, or a
//! plan-level term kept apart in [`LocalUsage`] and added by [`plan_cost`]:
//!
//! - CPU, scans, kept state (memory, disk), pane retention: per node. A
//!   chain's retention is read from the consumers' queries (their range
//!   spans), as Stage 3 reads it from the queries reaching it.
//! - Memory held while an evaluation runs (a query-time state, or a
//!   recomputed node's working memory): plan level ([`crate::HeldMemory`]),
//!   since it lasts the readers' whole latency; [`plan_cost`] prices it with
//!   each query's latency, the sum of its groups' work plus its longest
//!   compaction.
//! - Raw data on disk: plan level, each source's longest query-time scan
//!   ([`LocalUsage::raw_scans`]).
//! - A query's request overhead: per node, on the group that is its root.
//! - Compaction and partial copies of an ingestion-time state: per node,
//!   with the ingest workers counted from the private nodes' ingestion
//!   work only (exact while a state's whole ingestion work below it fits
//!   on one core, as in every current test; otherwise a lower bound).
//!
//! Known limits: summaries are sized for the failure probability only in
//! a group that is the root of every query computing it directly; a
//! query-time scan group priced on its own reads the longest window its
//! parent groups' logical nodes put on it.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_physical_asap_optimization::{optimize_physical_asap_assigned, PhysicalCandidate};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::{NonASAPOp, OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::{GroupId, ImplementationCandidate, LocalUsage, Memo};
use crate::cost::physical_operator_statistics::EdgeStatistics;
use crate::{
    accuracy_targets, price_each_node, price_private_nodes, shared_evaluation_rate,
    sized_for_failure_probability, CalibrationFit, CostModel, NodeConsumer, PlanningModels,
    PrivatePricing, SelectionError,
};

/// Who computes a group's private nodes: its consumers, as Stage 3's
/// `computations` derives them from the whole plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConsumerContext {
    pub consumers: Vec<Consumer>,
}

/// One consumer of a group.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Consumer {
    /// A query reaching the group through unmaterialized nodes only: at
    /// the query's rate, range steps included.
    Query { query: usize },
    /// A materialized ancestor: at its rate (1 at ingestion time, the
    /// queries reaching it together at query time, once per request when
    /// kept across evaluations). `queries` are the queries reaching it at
    /// query time (none at ingestion time), whose latency and range spans
    /// the group's held memory and pane retention read.
    Materialized {
        rate_per_second: f64,
        queries: Vec<usize>,
        at_ingestion: bool,
    },
}

/// A consumer as a cache key (its rate by bits).
type ConsumerKey = (Option<usize>, u64, Vec<usize>, bool);

impl Consumer {
    fn key(&self) -> ConsumerKey {
        match self {
            Consumer::Query { query } => (Some(*query), 0, Vec::new(), false),
            Consumer::Materialized {
                rate_per_second,
                queries,
                at_ingestion,
            } => (
                None,
                rate_per_second.to_bits(),
                queries.clone(),
                *at_ingestion,
            ),
        }
    }

    fn queries(&self) -> Vec<usize> {
        match self {
            Consumer::Query { query } => vec![*query],
            Consumer::Materialized { queries, .. } => queries.clone(),
        }
    }

    fn at_ingestion(&self) -> bool {
        matches!(
            self,
            Consumer::Materialized {
                at_ingestion: true,
                ..
            }
        )
    }
}

type PartKey = (usize, Materialization, Vec<usize>);
type PriceKey = (u32, Vec<ConsumerKey>, Materialization, Vec<(u64, u64)>);

/// The cached prices of one search over one memo: valid while the memo's
/// nodes live (parts are keyed by address), for one workload, data
/// workload and set of planning models, priced with one frozen
/// calibration factor.
#[derive(Debug, Default)]
pub(crate) struct PriceCache {
    /// The frozen calibration factor ([`frozen_calibration`]).
    formula_scale: Option<f64>,
    /// Structural node keys, interned.
    keys: HashMap<String, u32>,
    /// Each candidate's sub-DAG by (node address, materialization, the
    /// queries it is built for), with its nodes' keys.
    parts: HashMap<PartKey, Option<Rc<Part>>>,
    /// Each child node's sub-DAG's node keys and root key, by its address.
    child_keys: HashMap<usize, Rc<ChildKeys>>,
    /// Local prices by (candidate's structural key, consumers,
    /// materialization, children's outputs).
    prices: HashMap<PriceKey, Option<(LocalUsage, EdgeStatistics)>>,
    /// Local prices found in the cache.
    pub hits: u64,
    /// Local prices computed.
    pub misses: u64,
}

impl PriceCache {
    /// An empty cache pricing with the calibration factor `formula_scale`.
    pub(crate) fn new(formula_scale: f64) -> Self {
        Self {
            formula_scale: Some(formula_scale),
            ..Self::default()
        }
    }

    /// The interned structural node keys.
    pub(super) fn keys_mut(&mut self) -> &mut HashMap<String, u32> {
        &mut self.keys
    }
}

/// A candidate's sub-DAG as Stage 2 builds it, with its nodes' keys.
#[derive(Debug)]
struct Part {
    physical: PhysicalCandidate,
    keys: Vec<u32>,
}

/// A child group's sub-DAG's node keys, and its root's.
#[derive(Debug, Default)]
pub(super) struct ChildKeys {
    pub(super) nodes: HashSet<u32>,
    pub(super) root: Option<u32>,
}

/// The usage of `candidate`'s private nodes (from its node down to,
/// excluding, its child groups' nodes) computed for `consumers`,
/// materialized as `materialization` (which makes it the consumer of its
/// children), reading its children's outputs `child_stats` (in the order
/// of `candidate.children`). Cost model 1, the frozen calibration factor.
/// `None` when a check sharing cannot repair fails (a per-series aggregate
/// the executor lacks, a query-time scan of raw data the deployment does
/// not keep) or Stage 2 does not build it so.
#[allow(clippy::too_many_arguments)]
pub(crate) fn private_usage(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    consumers: &ConsumerContext,
    materialization: Materialization,
    child_stats: &[EdgeStatistics],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<Option<LocalUsage>, SelectionError> {
    Ok(priced(
        memo,
        candidate,
        consumers,
        materialization,
        child_stats,
        entries,
        data,
        models,
        cache,
    )?
    .map(|(usage, _)| usage))
}

/// `candidate`'s output per computation given its children's outputs
/// `child_stats`, computed at query time for its group's queries with
/// nothing materialized; `None` when it is not priced.
#[allow(clippy::too_many_arguments)]
pub(crate) fn output_stats(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    child_stats: &[EdgeStatistics],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<Option<EdgeStatistics>, SelectionError> {
    let group = &memo.groups[candidate.implements[0].0 as usize];
    let consumers = ConsumerContext {
        consumers: group
            .queries
            .iter()
            .map(|&query| Consumer::Query { query })
            .collect(),
    };
    Ok(priced(
        memo,
        candidate,
        &consumers,
        Materialization::NotMaterialized,
        child_stats,
        entries,
        data,
        models,
        cache,
    )?
    .map(|(_, output)| output))
}

#[allow(clippy::too_many_arguments)]
fn priced(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    consumers: &ConsumerContext,
    materialization: Materialization,
    child_stats: &[EdgeStatistics],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<Option<(LocalUsage, EdgeStatistics)>, SelectionError> {
    let group = candidate.implements[0];
    let roots_of = &memo.groups[group.0 as usize].roots_of;
    // The queries computing it directly; the part is built for them (one
    // root each) and sized for them when it is all their roots.
    let direct: Vec<usize> = consumers
        .consumers
        .iter()
        .filter_map(|c| match c {
            Consumer::Query { query } => Some(*query),
            _ => None,
        })
        .collect();
    let built_for = match direct.is_empty() {
        false => direct.clone(),
        true => memo.groups[group.0 as usize]
            .queries
            .first()
            .copied()
            .into_iter()
            .collect(),
    };
    let sized = !direct.is_empty()
        && direct.len() == consumers.consumers.len()
        && direct.iter().all(|q| roots_of.contains(q));
    let Some(part) = part(
        candidate,
        materialization,
        &built_for,
        sized,
        entries,
        data,
        &models,
        cache,
    ) else {
        return Ok(None);
    };
    let key: PriceKey = (
        root_key(&part.physical.dag, &part.keys).unwrap_or(u32::MAX),
        consumers.consumers.iter().map(Consumer::key).collect(),
        materialization,
        child_stats.iter().map(|s| (s.rows, s.bytes)).collect(),
    );
    if let Some(price) = cache.prices.get(&key) {
        cache.hits += 1;
        return Ok(price.clone());
    }
    cache.misses += 1;
    let price = price_part(
        memo,
        candidate,
        &part,
        consumers,
        child_stats,
        entries,
        data,
        &models,
        cache,
    )?;
    cache.prices.insert(key, price.clone());
    Ok(price)
}

/// The key of `dag`'s first root, `keys` being its nodes' by position.
fn root_key(dag: &PhysicalASAPDAG, keys: &[u32]) -> Option<u32> {
    let root = dag.roots.first()?;
    let i = dag.nodes.iter().position(|n| n.id == *root)?;
    Some(keys[i])
}

/// `candidate`'s sub-DAG, built by Stage 2 for the queries `built_for`
/// (one root each) with `materialization` on its first materialization
/// candidate (its topmost build, or its node when several read it) and
/// nothing on the others, sized for the queries' failure probabilities
/// when `sized`; cached.
#[allow(clippy::too_many_arguments)]
fn part(
    candidate: &ImplementationCandidate,
    materialization: Materialization,
    built_for: &[usize],
    sized: bool,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Option<Rc<Part>> {
    let mut queries = built_for.to_vec();
    if sized {
        queries.push(usize::MAX);
    }
    let key = (
        Rc::as_ptr(&candidate.node) as usize,
        materialization,
        queries,
    );
    if let Some(part) = cache.parts.get(&key) {
        return part.clone();
    }
    let part_entries: Vec<QueryWorkloadEntry> =
        built_for.iter().map(|&q| entries[q].clone()).collect();
    let physical = built(
        &candidate.node,
        &part_entries,
        data,
        models,
        materialization,
    )
    .map(|physical| match sized {
        true => sized_for_failure_probability(&physical, &accuracy_targets(&part_entries), data)
            .unwrap_or(physical),
        false => physical,
    });
    let part = physical.map(|physical| {
        let keys = keys_of(&physical.dag, &mut cache.keys);
        Rc::new(Part { physical, keys })
    });
    cache.parts.insert(key, part.clone());
    part
}

/// Stage 2's sub-DAG of `node`, one root per entry (two copies of a
/// single entry for `MatAtQuery`, which needs two readers), with
/// `materialization` on one materialization candidate and nothing on the
/// others: `MatAtQuery` on its node (the last candidate, the highest
/// shared node), any other on its topmost build (the first). `None` when
/// Stage 2 does not build that.
fn built(
    node: &Rc<OperatorNode>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    materialization: Materialization,
) -> Option<PhysicalCandidate> {
    let at_query = matches!(materialization, Materialization::MatAtQuery(_));
    let entries: Vec<QueryWorkloadEntry> = match (at_query, entries) {
        (true, [one]) => vec![one.clone(), one.clone()],
        _ => entries.to_vec(),
    };
    let roots = vec![QueryRoot::Operator(Rc::clone(node)); entries.len()];
    let build = |target: Option<usize>| {
        let mut index = 0;
        let mut choose = |_: &[usize]| {
            let chosen = match Some(index) == target {
                true => materialization,
                false => Materialization::NotMaterialized,
            };
            index += 1;
            Some(chosen)
        };
        let built = optimize_physical_asap_assigned(
            "S",
            &roots,
            &entries,
            data,
            models.deployment_capabilities,
            &mut choose,
        )
        .ok()
        .flatten();
        (built, index)
    };
    let (plain, candidates) = build(None);
    if materialization == Materialization::NotMaterialized {
        return plain;
    }
    let target = match at_query {
        true => candidates.checked_sub(1)?,
        false => 0,
    };
    let built = build(Some(target)).0?;
    built
        .dag
        .nodes
        .iter()
        .any(|n| n.materialization == materialization)
        .then_some(built)
}

/// The node keys of `child`'s own sub-DAG (one reader, nothing
/// materialized) and its root's key, cached by the child's node.
pub(super) fn child_keys(
    memo: &Memo,
    child: GroupId,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Rc<ChildKeys> {
    let node = &memo.groups[child.0 as usize].node;
    let address = Rc::as_ptr(node) as usize;
    if let Some(keys) = cache.child_keys.get(&address) {
        return Rc::clone(keys);
    }
    let reader_entries: Vec<QueryWorkloadEntry> = memo.groups[child.0 as usize]
        .queries
        .first()
        .map(|&q| entries[q].clone())
        .into_iter()
        .collect();
    let keys = built(
        node,
        &reader_entries,
        data,
        models,
        Materialization::NotMaterialized,
    )
    .map(|physical| {
        let keys = keys_of(&physical.dag, &mut cache.keys);
        ChildKeys {
            root: root_key(&physical.dag, &keys),
            nodes: keys.into_iter().collect(),
        }
    })
    .unwrap_or_default();
    let keys = Rc::new(keys);
    cache.child_keys.insert(address, Rc::clone(&keys));
    keys
}

/// Each node's structural key, by position: its payload over its
/// producers' keys, and its output schema; placement (timing,
/// materialization) left out, so a child's nodes match whatever is chosen
/// for them.
pub(super) fn keys_of(dag: &PhysicalASAPDAG, interned: &mut HashMap<String, u32>) -> Vec<u32> {
    let mut key_of: HashMap<PhysicalASAPNodeId, u32> = HashMap::new();
    dag.nodes
        .iter()
        .map(|node| {
            let payload = node.payload.map_children(|id| key_of.get(id).copied());
            let text = serde_json::to_string(&(&payload, &node.output_schema))
                .expect("a physical node serializes");
            let next = interned.len() as u32;
            let key = *interned.entry(text).or_insert(next);
            key_of.insert(node.id, key);
            key
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn price_part(
    memo: &Memo,
    candidate: &ImplementationCandidate,
    part: &Part,
    consumers: &ConsumerContext,
    child_stats: &[EdgeStatistics],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<Option<(LocalUsage, EdgeStatistics)>, SelectionError> {
    let dag = &part.physical.dag;
    // The children's nodes, and each child root's output.
    let mut child_nodes: HashSet<u32> = HashSet::new();
    let mut child_roots: Vec<Option<u32>> = Vec::new();
    for &child in &candidate.children {
        let keys = child_keys(memo, child, entries, data, models, cache);
        child_nodes.extend(keys.nodes.iter().copied());
        child_roots.push(keys.root);
    }
    let private: Vec<bool> = part.keys.iter().map(|k| !child_nodes.contains(k)).collect();
    let mut child_outputs = HashMap::new();
    for (node, key) in dag.nodes.iter().zip(&part.keys) {
        if let Some(i) = child_roots.iter().position(|k| *k == Some(*key)) {
            if let Some(stats) = child_stats.get(i) {
                child_outputs.insert(node.id, *stats);
            }
        }
    }
    // The queries the consumers name, by position.
    let mut queries: Vec<usize> = Vec::new();
    for consumer in &consumers.consumers {
        for q in consumer.queries() {
            if !queries.contains(&q) {
                queries.push(q);
            }
        }
    }
    let position = |q: usize| queries.iter().position(|&p| p == q).expect("named");
    let query_entries: Vec<QueryWorkloadEntry> =
        queries.iter().map(|&q| entries[q].clone()).collect();
    let roots_of = &memo.groups[candidate.implements[0].0 as usize].roots_of;
    let root_of: Vec<bool> = queries.iter().map(|q| roots_of.contains(q)).collect();
    let settings = models.plan_selection;
    let cost_error = |(node, error)| SelectionError::Cost {
        candidate: format!("group {}", candidate.implements[0].0),
        node,
        error,
    };
    let node_consumer = |consumer: &Consumer| -> Result<NodeConsumer, SelectionError> {
        Ok(match consumer {
            Consumer::Query { query } => NodeConsumer {
                rate_per_second: shared_evaluation_rate(&[&entries[*query]], settings)
                    .map_err(|error| cost_error((Default::default(), error)))?,
                queries: vec![position(*query)],
                at_ingestion: false,
            },
            Consumer::Materialized {
                rate_per_second,
                queries,
                at_ingestion,
            } => NodeConsumer {
                rate_per_second: *rate_per_second,
                queries: queries.iter().map(|&q| position(q)).collect(),
                at_ingestion: *at_ingestion,
            },
        })
    };
    // A copy at query time for the query-time consumers, one at ingestion
    // time for the others (Stage 2 builds one per timing).
    let mut usage = LocalUsage {
        cost_per_second: Some(0.0),
        cpu_ops_per_second: 0.0,
        scanned_bytes_per_second: 0.0,
        average_memory_bytes: 0.0,
        peak_memory_bytes: 0,
        disk_bytes: 0,
        latency_ms: queries.iter().map(|&q| (q, 0.0)).collect(),
        compaction_ms: queries.iter().map(|&q| (q, 0.0)).collect(),
        held_memory: Vec::new(),
        raw_scans: Vec::new(),
    };
    let mut output = None;
    for at_ingestion in [false, true] {
        let copy: Vec<NodeConsumer> = consumers
            .consumers
            .iter()
            .filter(|c| c.at_ingestion() == at_ingestion)
            .map(node_consumer)
            .collect::<Result<_, _>>()?;
        if copy.is_empty() {
            continue;
        }
        let input = PrivatePricing {
            dag,
            private: &private,
            child_outputs: &child_outputs,
            consumers: &copy,
            queries: &query_entries,
            root_of: &root_of,
            scan_lookback_ms: scan_lookback_ms(memo, candidate.implements[0]),
            data,
            settings,
            formula_scale: cache.formula_scale.unwrap_or(1.0),
        };
        let Some(nodes) =
            price_private_nodes(&input, models.deployment_capabilities.raw_data_retained)
                .map_err(cost_error)?
        else {
            return Ok(None);
        };
        usage.cost_per_second = match (usage.cost_per_second, nodes.cost_per_second) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        usage.cpu_ops_per_second += nodes.cpu_ops_per_second;
        usage.scanned_bytes_per_second += nodes.scanned_bytes_per_second;
        usage.average_memory_bytes += nodes.average_memory_bytes;
        usage.peak_memory_bytes = usage
            .peak_memory_bytes
            .saturating_add(nodes.peak_memory_bytes);
        usage.disk_bytes = usage.disk_bytes.saturating_add(nodes.disk_bytes);
        for (i, ms) in nodes.query_ms.iter().enumerate() {
            usage.latency_ms[i].1 += ms;
        }
        for (i, ms) in nodes.compaction_ms.iter().enumerate() {
            usage.compaction_ms[i].1 = usage.compaction_ms[i].1.max(*ms);
        }
        usage
            .held_memory
            .extend(nodes.held.into_iter().map(|mut held| {
                for (reader, _) in &mut held.readers {
                    *reader = queries[*reader];
                }
                held
            }));
        usage.raw_scans.extend(nodes.raw_scans);
        output.get_or_insert(nodes.output);
    }
    Ok(output.map(|output| (usage, output)))
}

/// The raw data a query-time scan group priced on its own reads: the
/// longest window its parent groups' nodes put on it (through time
/// shifts); `None` when it is not a scan or no window bounds it.
pub(super) fn scan_lookback_ms(memo: &Memo, group: GroupId) -> Option<u64> {
    let scan = &memo.groups[group.0 as usize];
    if !matches!(scan.node.non_asap(), Some(NonASAPOp::Scan { .. })) {
        return None;
    }
    let mut windows = Vec::new();
    let mut stack = scan.parents.clone();
    let mut seen = HashSet::new();
    while let Some(g) = stack.pop() {
        if !seen.insert(g) {
            continue;
        }
        let parent = &memo.groups[g.0 as usize];
        match parent.node.non_asap() {
            Some(NonASAPOp::TimeRange { range, .. }) => windows.push(range.as_millis() as u64),
            Some(NonASAPOp::TimeShift { .. }) => stack.extend(parent.parents.iter().copied()),
            _ => {}
        }
    }
    windows.into_iter().max()
}

/// The whole plan's price per second under cost model 1 from its groups'
/// local usages: their prices, plus the memory each holds while an
/// evaluation runs (each query's latency being the sum of its groups'
/// work plus its longest compaction), plus each source's longest
/// query-time raw scan kept on disk. `None` under cost models 2 and 3.
pub(crate) fn plan_cost<'u>(
    parts: impl IntoIterator<Item = &'u LocalUsage> + Clone,
    cost_model: &CostModel,
) -> Option<f64> {
    let CostModel::UsageOverTime(prices) = cost_model else {
        return None;
    };
    let mut cost = 0.0;
    let mut work: HashMap<usize, f64> = HashMap::new();
    let mut compaction: HashMap<usize, f64> = HashMap::new();
    let mut raw: HashMap<&str, u64> = HashMap::new();
    for part in parts.clone() {
        cost += part.cost_per_second?;
        for &(q, ms) in &part.latency_ms {
            *work.entry(q).or_default() += ms;
        }
        for &(q, ms) in &part.compaction_ms {
            let longest = compaction.entry(q).or_default();
            *longest = longest.max(ms);
        }
        for (source, bytes) in &part.raw_scans {
            let longest = raw.entry(source.as_str()).or_default();
            *longest = (*longest).max(*bytes);
        }
    }
    let latency_ms = |q: usize| {
        work.get(&q).copied().unwrap_or(0.0) + compaction.get(&q).copied().unwrap_or(0.0)
    };
    for part in parts {
        for held in &part.held_memory {
            cost += prices.cost_per_memory_byte_second * held.byte_seconds(latency_ms);
        }
    }
    cost += raw
        .values()
        .map(|&bytes| prices.cost_per_disk_byte_second * bytes as f64)
        .sum::<f64>();
    Some(cost)
}

/// The calibration factor of the formulas' CPU time to a measured cost
/// table, fitted once before the search over every candidate's private
/// nodes (each candidate's sub-DAG built for its group's queries with
/// nothing materialized) and frozen (design "Coupled charges"); 1 without
/// a measured table.
pub(crate) fn frozen_calibration(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<f64, SelectionError> {
    let settings = models.plan_selection;
    if settings.measured_operation_costs.operators.is_empty() {
        return Ok(1.0);
    }
    let mut fit = CalibrationFit::default();
    let mut cache = PriceCache::new(1.0);
    for (g, candidates) in memo.candidates.iter().enumerate() {
        let queries = memo.groups[g].queries.clone();
        let part_entries: Vec<QueryWorkloadEntry> =
            queries.iter().map(|&q| entries[q].clone()).collect();
        let recurrence: Vec<_> = part_entries.iter().map(|e| e.recurrence.clone()).collect();
        let ranges: Vec<_> = part_entries.iter().map(|e| e.range_evaluation).collect();
        for candidate in candidates {
            let Some(part) = part(
                candidate,
                Materialization::NotMaterialized,
                &queries,
                false,
                entries,
                data,
                &models,
                &mut cache,
            ) else {
                continue;
            };
            let mut children = HashSet::new();
            for &child in &candidate.children {
                let keys = child_keys(memo, child, entries, data, &models, &mut cache);
                children.extend(keys.nodes.iter().copied());
            }
            let dag = &part.physical.dag;
            let nodes = dag.nodes.iter().map(|n| (n.id, n)).collect();
            let each = price_each_node(
                dag,
                &nodes,
                &HashSet::new(),
                &recurrence,
                &ranges,
                data,
                settings,
                1.0,
                None,
                None,
            )
            .map_err(|(node, error)| SelectionError::Cost {
                candidate: format!("group {g}"),
                node,
                error,
            })?;
            for (node, key) in dag.nodes.iter().zip(&part.keys) {
                if !children.contains(key) {
                    fit.add(each.priced[&node.id].fit);
                }
            }
        }
    }
    Ok(fit.factor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::subexpr::groups::memo_of;
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::physical::Medium;
    use asap_types::workload::{
        DataArrival, QueryRecurrence, RepeatedDemand, RepetitionInterval, SourceFacts,
    };

    fn data() -> DataWorkload {
        DataWorkload {
            arrival: DataArrival::Ingesting {
                rows_per_second: 10_000.0,
            },
            sources: [(
                "m".to_string(),
                SourceFacts {
                    series_count: Some(10_000),
                    ..SourceFacts::default()
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    /// Queries with exact targets repeating every
    /// `intervals_ms`.
    fn entries(intervals_ms: &[u32]) -> Vec<QueryWorkloadEntry> {
        intervals_ms
            .iter()
            .map(|&interval| {
                entry_with(
                    AccuracyTarget::exact(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        interval,
                    ))),
                )
            })
            .collect()
    }

    /// The memo of PromQL `queries` with exact targets.
    fn memo(queries: &[&str]) -> Memo {
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .map(|q| QueryRoot::Operator(lower_promql(q, AccuracyTarget::exact())))
            .enumerate()
            .collect();
        memo_of(&generate_per_computation_candidates(roots).unwrap())
    }

    /// The candidate chosen for group `g`: its first.
    fn own(memo: &Memo, g: usize) -> &ImplementationCandidate {
        &memo.candidates[g][0]
    }

    /// Group `g`'s chosen candidate over its children's, composed into one
    /// node (each group composed once, so a shared group is one node).
    fn composed(
        memo: &Memo,
        g: usize,
        done: &mut HashMap<usize, Rc<OperatorNode>>,
    ) -> Rc<OperatorNode> {
        if let Some(node) = done.get(&g) {
            return Rc::clone(node);
        }
        let candidate = own(memo, g);
        let children: Vec<(Rc<OperatorNode>, Rc<OperatorNode>)> = candidate
            .children
            .iter()
            .map(|c| {
                let c = c.0 as usize;
                (Rc::clone(&memo.groups[c].node), composed(memo, c, done))
            })
            .collect();
        fn rebuild(
            node: &Rc<OperatorNode>,
            children: &[(Rc<OperatorNode>, Rc<OperatorNode>)],
        ) -> Rc<OperatorNode> {
            if let Some((_, child)) = children.iter().find(|(group, _)| **group == **node) {
                return Rc::clone(child);
            }
            if node.children().is_empty() {
                return Rc::clone(node);
            }
            Rc::new(
                node.with_new_children(|child| rebuild(child, children))
                    .expect("rebuilds")
                    .with_timing(node.timing)
                    .with_guarantee(node.guarantee.clone()),
            )
        }
        let node = rebuild(&candidate.node, &children);
        done.insert(g, Rc::clone(&node));
        node
    }

    /// Stage 3's price ([`crate::price_at_scale`]) of the whole plan of `memo`'s roots, each group of
    /// `materialized` materialized as given (as Stage 2's materialization
    /// candidate its readers name), nothing else.
    fn stage3_price(
        memo: &Memo,
        entries: &[QueryWorkloadEntry],
        materialized: &[(usize, Materialization)],
    ) -> f64 {
        let data = data();
        let models = PlanningModels::builtin();
        let mut done = HashMap::new();
        let roots: Vec<QueryRoot> = (0..entries.len())
            .map(|q| {
                let g = memo
                    .groups
                    .iter()
                    .position(|group| group.roots_of.contains(&q))
                    .unwrap();
                QueryRoot::Operator(composed(memo, g, &mut done))
            })
            .collect();
        // Stage 2's materialization candidates, in discovery order: builds
        // first, then each highest shared node, which a group materialized
        // at query time is (the last one its readers name).
        let mut readers_of: Vec<Vec<usize>> = Vec::new();
        let mut record = |readers: &[usize]| {
            readers_of.push(readers.to_vec());
            Some(Materialization::NotMaterialized)
        };
        optimize_physical_asap_assigned(
            "L1",
            &roots,
            entries,
            &data,
            models.deployment_capabilities,
            &mut record,
        )
        .unwrap();
        let chosen: Vec<Materialization> = (0..readers_of.len())
            .map(|i| {
                materialized
                    .iter()
                    .find(|(g, _)| {
                        let queries = &memo.groups[*g].queries;
                        readers_of.iter().rposition(|r| r == queries) == Some(i)
                    })
                    .map_or(Materialization::NotMaterialized, |(_, m)| *m)
            })
            .collect();
        let mut next = chosen.into_iter();
        let mut choose = |_: &[usize]| next.next();
        let candidate = optimize_physical_asap_assigned(
            "L1",
            &roots,
            entries,
            &data,
            models.deployment_capabilities,
            &mut choose,
        )
        .unwrap()
        .unwrap();
        let dag = &candidate.dag;
        let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
        let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
        let (cost, _) = crate::price_at_scale(
            dag,
            &crate::node_map(dag),
            &crate::kept_pane_chains(dag),
            &recurrence,
            &ranges,
            &data,
            models.plan_selection,
            1.0,
        )
        .unwrap();
        cost.cost_per_second.unwrap()
    }

    /// Every group's own candidate priced in its true context, as the
    /// search fixes it: outputs bottom-up, consumers top-down (each query
    /// at its root; a parent passes its consumers on, or is the one
    /// consumer when materialized), the groups of `materialized`
    /// materialized as given.
    fn local_prices(
        memo: &Memo,
        entries: &[QueryWorkloadEntry],
        materialized: &[(usize, Materialization)],
        cache: &mut PriceCache,
    ) -> Vec<LocalUsage> {
        let data = data();
        let models = PlanningModels::builtin();
        let settings = models.plan_selection;
        let materialization_of = |g: usize| {
            materialized
                .iter()
                .find(|(m, _)| *m == g)
                .map_or(Materialization::NotMaterialized, |(_, m)| *m)
        };
        let child_stats = |outputs: &[EdgeStatistics], g: usize| -> Vec<EdgeStatistics> {
            own(memo, g)
                .children
                .iter()
                .map(|c| outputs[c.0 as usize])
                .collect()
        };
        let mut outputs: Vec<EdgeStatistics> = Vec::new();
        for g in 0..memo.groups.len() {
            let stats = child_stats(&outputs, g);
            let output = output_stats(memo, own(memo, g), &stats, entries, &data, models, cache)
                .unwrap()
                .expect("an output");
            outputs.push(output);
        }
        let mut consumers: Vec<Vec<Consumer>> = vec![Vec::new(); memo.groups.len()];
        for g in (0..memo.groups.len()).rev() {
            let group = &memo.groups[g];
            for &q in &group.roots_of {
                consumers[g].push(Consumer::Query { query: q });
            }
            let passed: Vec<Consumer> = match materialization_of(g) {
                Materialization::NotMaterialized => consumers[g].clone(),
                m => {
                    let at_ingestion = matches!(m, Materialization::MatAtIngestion(_));
                    let reaching: Vec<&QueryWorkloadEntry> =
                        group.queries.iter().map(|&q| &entries[q]).collect();
                    vec![Consumer::Materialized {
                        rate_per_second: match at_ingestion {
                            true => 1.0,
                            false => shared_evaluation_rate(&reaching, settings).unwrap(),
                        },
                        queries: match at_ingestion {
                            true => Vec::new(),
                            false => group.queries.clone(),
                        },
                        at_ingestion,
                    }]
                }
            };
            for &child in &own(memo, g).children {
                consumers[child.0 as usize].extend(passed.iter().cloned());
            }
        }
        (0..memo.groups.len())
            .map(|g| {
                private_usage(
                    memo,
                    own(memo, g),
                    &ConsumerContext {
                        consumers: consumers[g].clone(),
                    },
                    materialization_of(g),
                    &child_stats(&outputs, g),
                    entries,
                    &data,
                    models,
                    cache,
                )
                .unwrap()
                .unwrap_or_else(|| panic!("group {g} is priced"))
            })
            .collect()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-9 * expected.abs(),
            "{actual} vs {expected}"
        );
    }

    /// The groups' local prices of `queries`' plan, `materialized` as
    /// given, add up to Stage 3's price of the whole plan.
    fn check(
        queries: &[&str],
        intervals_ms: &[u32],
        materialized: impl Fn(&Memo) -> Vec<(usize, Materialization)>,
    ) {
        let memo = memo(queries);
        let entries = entries(intervals_ms);
        let materialized = materialized(&memo);
        let mut cache = PriceCache::new(1.0);
        let parts = local_prices(&memo, &entries, &materialized, &mut cache);
        let model = &PlanningModels::builtin().plan_selection.cost_model;
        let expected = stage3_price(&memo, &entries, &materialized);
        assert_close(plan_cost(&parts, model).unwrap(), expected);
    }

    /// The highest group both queries read.
    fn shared(memo: &Memo) -> usize {
        memo.groups
            .iter()
            .rposition(|g| g.queries.len() == 2)
            .expect("a shared group")
    }

    #[test]
    fn local_prices_add_up_to_a_one_query_plans_price() {
        for query in [
            "sum by (job) (rate(m[5m]))",
            "max(avg_over_time(m[10m]))",
            "topk(3, sum by (job) (rate(m[5m])))",
            "sum(m offset 5m)",
            "count by (job) (max_over_time(m[1h]))",
        ] {
            check(&[query], &[30_000], |_| Vec::new());
        }
    }

    /// The per-job rate sum both queries read is one group: computed per
    /// reader when not materialized, once for both when materialized at
    /// query time, its inputs with it.
    #[test]
    fn local_prices_add_up_with_a_shared_group() {
        let queries = [
            "sum by (job) (rate(m[5m]))",
            "sum by (job) (rate(m[5m])) * 2",
        ];
        let memo = memo(&queries);
        let entries = entries(&[60_000, 60_000]);
        let at_query = [(shared(&memo), Materialization::MatAtQuery(Medium::Memory))];
        assert!(stage3_price(&memo, &entries, &[]) != stage3_price(&memo, &entries, &at_query));
        check(&queries, &[60_000, 60_000], |_| Vec::new());
        check(&queries, &[60_000, 60_000], |memo| {
            vec![(shared(memo), Materialization::MatAtQuery(Medium::Memory))]
        });
        check(&queries, &[60_000, 15_000], |memo| {
            vec![(shared(memo), Materialization::MatAtQuery(Medium::Memory))]
        });
        // A shared group under each query's own aggregate, materialized or not.
        let queries = ["max(rate(m[5m]))", "min(rate(m[5m]))"];
        check(&queries, &[60_000, 30_000], |_| Vec::new());
        check(&queries, &[60_000, 30_000], |memo| {
            vec![(shared(memo), Materialization::MatAtQuery(Medium::Memory))]
        });
    }

    /// Materialized, the shared group's input runs half as often for two
    /// readers evaluating together.
    #[test]
    fn a_materialized_shared_group_computes_its_input_once() {
        let memo = memo(&[
            "sum by (job) (rate(m[5m]))",
            "sum by (job) (rate(m[5m])) * 2",
        ]);
        let entries = entries(&[60_000, 60_000]);
        let s = shared(&memo);
        let below = memo.groups[s].children[0].0 as usize;
        let mut cache = PriceCache::new(1.0);
        let apart = local_prices(&memo, &entries, &[], &mut cache);
        let together = local_prices(
            &memo,
            &entries,
            &[(s, Materialization::MatAtQuery(Medium::Memory))],
            &mut cache,
        );
        assert_close(
            together[below].cpu_ops_per_second * 2.0,
            apart[below].cpu_ops_per_second,
        );
    }

    /// A tumbling-pane chain built at ingestion time for a 5-minute sum
    /// every minute: its local price is Stage 3's price of the plan.
    #[test]
    fn a_pane_chain_at_ingestion_is_priced_as_stage_3_prices_it() {
        use crate::pass::subexpr::windows::add_window_composition;
        use crate::pass::subexpr::{CandidateOrigin, MemoGroup};
        use asap_logical_asap_optimization::workload_candidates::stage1_logical_asap_optimization;
        use asap_types::ir::schema::schema_support::with_promql_series_identity;
        use asap_types::workload::Predictability;

        let mut entries = entries(&[60_000]);
        entries[0].predictability = Predictability::Predictable { known_at: None };
        let node = with_promql_series_identity(&lower_promql(
            "sum_over_time(m[5m])",
            AccuracyTarget::exact(),
        ))
        .unwrap();
        let inventory =
            generate_per_computation_candidates(vec![(0, QueryRoot::Operator(Rc::clone(&node)))])
                .unwrap();
        let candidates = stage1_logical_asap_optimization(&inventory, &entries)
            .filter(|c| c.pass2.is_none())
            .filter_map(|c| match c.roots.ok()?.pop()? {
                QueryRoot::Operator(node) => Some(node),
                QueryRoot::Scalar(_) => None,
            })
            .map(|node| ImplementationCandidate {
                implements: vec![GroupId(0)],
                node,
                children: Vec::new(),
                origin: CandidateOrigin::Pass1,
                coupling_sites: Vec::new(),
                materializations: Vec::new(),
                budget_share: 1,
                requires_exact_children: Vec::new(),
                rollup_of: None,
                requires_sites: Vec::new(),
            })
            .collect();
        let mut memo = Memo {
            groups: vec![MemoGroup {
                node,
                children: Vec::new(),
                parents: Vec::new(),
                queries: vec![0],
                roots_of: vec![0],
            }],
            candidates: vec![candidates],
        };
        add_window_composition(&mut memo, &entries);
        let panes = memo.candidates[0]
            .iter()
            .find(|c| c.origin == CandidateOrigin::Panes && c.coupling_sites.is_empty())
            .expect("a pane candidate");
        let at_ingestion = Materialization::MatAtIngestion(Medium::Memory);
        let data = data();
        let models = PlanningModels::builtin();
        let mut cache = PriceCache::new(1.0);
        let local = private_usage(
            &memo,
            panes,
            &ConsumerContext {
                consumers: vec![Consumer::Query { query: 0 }],
            },
            at_ingestion,
            &[],
            &entries,
            &data,
            models,
            &mut cache,
        )
        .unwrap()
        .expect("priced");
        let mut first = true;
        let mut choose = |_: &[usize]| {
            let chosen = match first {
                true => at_ingestion,
                false => Materialization::NotMaterialized,
            };
            first = false;
            Some(chosen)
        };
        let plan = optimize_physical_asap_assigned(
            "L1",
            &[QueryRoot::Operator(Rc::clone(&panes.node))],
            &entries,
            &data,
            models.deployment_capabilities,
            &mut choose,
        )
        .unwrap()
        .unwrap();
        assert!(plan
            .dag
            .nodes
            .iter()
            .any(|n| n.materialization == at_ingestion));
        let dag = &plan.dag;
        let (cost, _) = crate::price_at_scale(
            dag,
            &crate::node_map(dag),
            &crate::kept_pane_chains(dag),
            &[entries[0].recurrence.clone()],
            &[entries[0].range_evaluation],
            &data,
            models.plan_selection,
            1.0,
        )
        .unwrap();
        let model = &models.plan_selection.cost_model;
        assert_close(
            plan_cost([&local], model).unwrap(),
            cost.cost_per_second.unwrap(),
        );
    }

    #[test]
    fn the_cache_hits_on_a_repeated_context() {
        let memo = memo(&["sum by (job) (rate(m[5m]))"]);
        let entries = entries(&[60_000]);
        let mut cache = PriceCache::new(1.0);
        let first = local_prices(&memo, &entries, &[], &mut cache);
        let (misses, hits) = (cache.misses, cache.hits);
        let second = local_prices(&memo, &entries, &[], &mut cache);
        assert_eq!(first, second);
        assert_eq!(cache.misses, misses);
        assert_eq!(cache.hits, hits + 2 * memo.groups.len() as u64);
    }
}
