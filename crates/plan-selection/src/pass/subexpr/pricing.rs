//! Local prices per (group, candidate, context), cached, the frozen
//! calibration factor, and frontiers (design "Memo-group search": Local
//! price, Frontier, Coupled charges).
//!
//! A candidate's local price is the Stage 3 usage of its private nodes:
//! from its node down to, but excluding, its child groups' nodes. It is
//! priced as the difference of two Stage 3 prices of whole sub-DAGs, each
//! built by Stage 2 and priced by [`crate::price_part`] with the readers'
//! entries:
//!
//! - the candidate's node with everything below it, its children being the
//!   child groups' nodes, so its private nodes see their children's output
//!   statistics and the readers' rates;
//! - minus each child group's node with everything below it, priced alike.
//!
//! Additive usage (price under cost model 1, CPU, scans, average memory,
//! peak memory, disk) is the difference; peak cores are the candidate's
//! whole sub-DAG's (frontiers combine them by maximum); each reader's
//! latency is what the candidate adds to its longest child's. Summed over a
//! plan's groups, the differences telescope to Stage 3's price of the whole
//! plan, coupled charges included (memory held for a query's whole latency,
//! a source's longest raw scan), which the root's sub-DAG carries.
//!
//! Materialization: the context's materializations, in order, are the
//! options of the sub-DAG's builds in Stage 2's discovery order; builds
//! past the list are `NotMaterialized`. A child's builds are the trailing
//! ones of its parent's sub-DAG.

use std::collections::HashMap;

use asap_physical_asap_optimization::{optimize_physical_asap_assigned, PhysicalCandidate};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};
use std::rc::Rc;

use super::{Context, GroupId, ImplementationCandidate, LocalUsage, Memo};
use crate::{
    price_part, CalibrationFit, CandidateCost, ClusterLimits, CostModel, PlanningModels,
    SelectionError,
};

/// The cached prices of one search: valid for one workload, data workload
/// and set of planning models, priced with one frozen calibration factor.
#[derive(Debug)]
pub(crate) struct PriceCache {
    /// The frozen calibration factor ([`frozen_calibration`]).
    formula_scale: f64,
    /// Local prices by (group, candidate's node, context, readers).
    local: HashMap<LocalKey, Option<LocalUsage>>,
    /// Whole sub-DAG prices by (node, readers, materializations).
    trees: HashMap<TreeKey, Option<TreePrice>>,
    /// Local prices found in the cache.
    pub hits: u64,
    /// Local prices computed.
    pub misses: u64,
}

impl PriceCache {
    /// An empty cache pricing with the calibration factor `formula_scale`.
    pub(crate) fn new(formula_scale: f64) -> Self {
        Self {
            formula_scale,
            local: HashMap::new(),
            trees: HashMap::new(),
            hits: 0,
            misses: 0,
        }
    }
}

/// A node by its structure: its serialized tree (a hash alone does not
/// prove two nodes equal).
type NodeKey = String;
type LocalKey = (GroupId, NodeKey, Context, Vec<usize>);
type TreeKey = (NodeKey, Vec<usize>, Vec<Materialization>);

/// One whole sub-DAG's Stage 3 price.
#[derive(Debug, Clone, PartialEq)]
struct TreePrice {
    usage: LocalUsage,
    /// The builds Stage 2 found in it, in discovery order.
    builds: usize,
}

fn node_key(node: &OperatorNode) -> NodeKey {
    serde_json::to_string(node).expect("an operator node serializes")
}

/// `candidate` of `group` priced in `context` for the queries `readers` (by
/// index in the workload, whose `entries` they are): the Stage 3 usage of
/// its private nodes (module doc). `None` when it fails a check sharing
/// cannot repair, or Stage 2 does not build it with the context's
/// materializations.
#[allow(clippy::too_many_arguments)]
pub(crate) fn local_price(
    memo: &Memo,
    group: GroupId,
    candidate: &ImplementationCandidate,
    context: &Context,
    readers: &[usize],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<Option<LocalUsage>, SelectionError> {
    let key = (
        group,
        node_key(&candidate.node),
        context.clone(),
        readers.to_vec(),
    );
    if let Some(price) = cache.local.get(&key) {
        cache.hits += 1;
        return Ok(price.clone());
    }
    cache.misses += 1;
    let reader_entries: Vec<QueryWorkloadEntry> =
        readers.iter().map(|&q| entries[q].clone()).collect();
    let materializations: Vec<Materialization> =
        context.materialization.iter().map(|(_, m)| *m).collect();
    let pricing = TreePricing {
        readers,
        entries: &reader_entries,
        data,
        models: &models,
    };
    let Some(whole) = pricing.price(&candidate.node, &materializations, cache)? else {
        cache.local.insert(key, None);
        return Ok(None);
    };
    let mut children = Vec::with_capacity(candidate.children.len());
    // Each child's builds trail its parent's, in child order.
    let mut end = whole.builds;
    let child_builds: Vec<usize> = candidate
        .children
        .iter()
        .map(|c| pricing.builds(&memo.groups[c.0 as usize].node))
        .collect();
    let mut offsets = vec![0; child_builds.len()];
    for (i, builds) in child_builds.iter().enumerate().rev() {
        end = end.saturating_sub(*builds);
        offsets[i] = end;
    }
    for (child, (&offset, &builds)) in candidate
        .children
        .iter()
        .zip(offsets.iter().zip(&child_builds))
    {
        let node = &memo.groups[child.0 as usize].node;
        let own = materializations
            .get(offset..(offset + builds).min(materializations.len()))
            .unwrap_or(&[]);
        // A child that does not price alone (e.g. a raw scan only its
        // parent's ingestion build makes valid) is charged inside its
        // parent: nothing is subtracted for it.
        if let Some(price) = pricing.price(node, own, cache)? {
            children.push(price.usage);
        }
    }
    let local = difference(&whole.usage, &children, &models.plan_selection.cost_model);
    cache.local.insert(key, Some(local.clone()));
    Ok(Some(local))
}

/// Pricing whole sub-DAGs for one set of readers.
struct TreePricing<'a> {
    readers: &'a [usize],
    entries: &'a [QueryWorkloadEntry],
    data: &'a DataWorkload,
    models: &'a PlanningModels<'a>,
}

impl TreePricing<'_> {
    /// Stage 2's sub-DAG of `node`, one root per reader, its builds given
    /// `materializations` in order (`NotMaterialized` past them), and the
    /// number of builds; `None` when Stage 2 does not build it so.
    fn built(
        &self,
        node: &Rc<OperatorNode>,
        materializations: &[Materialization],
    ) -> Option<(PhysicalCandidate, usize)> {
        let roots = vec![QueryRoot::Operator(Rc::clone(node)); self.readers.len()];
        let mut next = materializations.iter().copied();
        let mut builds = 0;
        let mut choose = |_: &[usize]| {
            builds += 1;
            Some(next.next().unwrap_or(Materialization::NotMaterialized))
        };
        let built = optimize_physical_asap_assigned(
            "S",
            &roots,
            self.entries,
            self.data,
            self.models.deployment_capabilities,
            &mut choose,
        );
        let physical = built.ok().flatten()?;
        Some((physical, builds))
    }

    /// The builds Stage 2 finds in `node`'s sub-DAG.
    fn builds(&self, node: &Rc<OperatorNode>) -> usize {
        self.built(node, &[]).map_or(0, |(_, builds)| builds)
    }

    /// `node`'s whole sub-DAG priced by Stage 3, cached.
    fn price(
        &self,
        node: &Rc<OperatorNode>,
        materializations: &[Materialization],
        cache: &mut PriceCache,
    ) -> Result<Option<TreePrice>, SelectionError> {
        let key = (
            node_key(node),
            self.readers.to_vec(),
            materializations.to_vec(),
        );
        if let Some(price) = cache.trees.get(&key) {
            return Ok(price.clone());
        }
        let price = match self.built(node, materializations) {
            None => None,
            Some((physical, builds)) => price_part(
                &physical,
                self.entries,
                self.data,
                self.models,
                cache.formula_scale,
            )?
            .map(|(cost, _)| TreePrice {
                usage: usage_of(&cost, self.readers),
                builds,
            }),
        };
        cache.trees.insert(key, price.clone());
        Ok(price)
    }
}

/// `cost` as a usage, each root's latency that of the reader at its index.
fn usage_of(cost: &CandidateCost, readers: &[usize]) -> LocalUsage {
    let usage = &cost.usage;
    LocalUsage {
        cost_per_second: cost.cost_per_second,
        cpu_ops_per_second: usage.cpu_ops_per_second,
        scanned_bytes_per_second: usage.scanned_bytes_per_second,
        average_memory_bytes: usage.average_memory_bytes,
        peak_memory_bytes: usage.peak_memory_bytes,
        peak_cpu_cores: usage.peak_cpu_cores,
        disk_bytes: usage.disk_bytes,
        latency_ms: readers
            .iter()
            .zip(&usage.latency_per_query)
            .map(|(&q, latency)| (q, latency.total_ms()))
            .collect(),
    }
}

/// `whole` minus `children`: additive components subtracted, peak cores
/// `whole`'s, each reader's latency over its longest child's. Under cost
/// model 2 the price is recomputed from the difference's peaks.
fn difference(whole: &LocalUsage, children: &[LocalUsage], cost_model: &CostModel) -> LocalUsage {
    let mut local = whole.clone();
    for child in children {
        local.cost_per_second = match (local.cost_per_second, child.cost_per_second) {
            (Some(a), Some(b)) => Some(a - b),
            (a, _) => a,
        };
        local.cpu_ops_per_second -= child.cpu_ops_per_second;
        local.scanned_bytes_per_second -= child.scanned_bytes_per_second;
        local.average_memory_bytes -= child.average_memory_bytes;
        local.peak_memory_bytes = local
            .peak_memory_bytes
            .saturating_sub(child.peak_memory_bytes);
        local.disk_bytes = local.disk_bytes.saturating_sub(child.disk_bytes);
    }
    for (query, latency) in &mut local.latency_ms {
        let longest_child = children
            .iter()
            .flat_map(|c| &c.latency_ms)
            .filter(|(q, _)| q == query)
            .map(|(_, l)| *l)
            .fold(0.0, f64::max);
        *latency -= longest_child;
    }
    if let CostModel::ProvisionedCapacity(_) = cost_model {
        local.cost_per_second = provisioned_cost(&local, cost_model);
    }
    local
}

/// The calibration factor of the formulas' CPU time to a measured cost
/// table, fitted once over every candidate of every group, each with all
/// its group's queries as readers, before the search, and frozen (design
/// "Coupled charges"); 1 without a measured table.
pub(crate) fn frozen_calibration(
    memo: &Memo,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
) -> Result<f64, SelectionError> {
    if models
        .plan_selection
        .measured_operation_costs
        .operators
        .is_empty()
    {
        return Ok(1.0);
    }
    let mut fit = CalibrationFit::default();
    for (group, candidates) in memo.groups.iter().zip(&memo.candidates) {
        let reader_entries: Vec<QueryWorkloadEntry> =
            group.queries.iter().map(|&q| entries[q].clone()).collect();
        let pricing = TreePricing {
            readers: &group.queries,
            entries: &reader_entries,
            data,
            models: &models,
        };
        for candidate in candidates {
            if let Some((physical, _)) = pricing.built(&candidate.node, &[]) {
                if let Some((_, candidate_fit)) =
                    price_part(&physical, &reader_entries, data, &models, 1.0)?
                {
                    fit.add(candidate_fit);
                }
            }
        }
    }
    Ok(fit.factor())
}

/// Under cost model 2, `usage`'s provisioned price; otherwise its own.
fn provisioned_cost(usage: &LocalUsage, cost_model: &CostModel) -> Option<f64> {
    match cost_model {
        CostModel::ProvisionedCapacity(prices) => Some(
            prices.cost_per_provisioned_memory_byte_second * usage.peak_memory_bytes as f64
                + prices.cost_per_provisioned_cpu_core_second * f64::from(usage.peak_cpu_cores)
                + prices.cost_per_disk_byte_second * usage.disk_bytes as f64,
        ),
        _ => usage.cost_per_second,
    }
}

/// The candidates of a subproblem no other dominates in usage: under cost
/// model 1 the cheapest (Stage 3's tie tolerance; ties keep the earlier),
/// under cost models 2 and 3 the Pareto front over (price, peak memory,
/// peak cores, disk, worst latency), exact ties kept. Under cost model 3,
/// usage over the cluster's limits is never kept.
#[derive(Debug, Clone)]
pub(crate) struct Frontier<T> {
    cost_model: CostModel,
    /// The kept usages with what each stands for, in insertion order.
    pub entries: Vec<(LocalUsage, T)>,
}

impl<T: Clone> Frontier<T> {
    pub(crate) fn new(cost_model: &CostModel) -> Self {
        Self {
            cost_model: cost_model.clone(),
            entries: Vec::new(),
        }
    }

    /// Add `usage` standing for `item` unless a kept entry dominates it,
    /// dropping the kept entries it dominates; whether it was kept.
    pub(crate) fn insert(&mut self, usage: LocalUsage, item: T) -> bool {
        if let CostModel::ClusterCapacity(limits) = &self.cost_model {
            if over_limits(&usage, limits) {
                return false;
            }
        }
        if let CostModel::UsageOverTime(_) = self.cost_model {
            let price = usage.cost_per_second.unwrap_or(f64::INFINITY);
            match self.entries.first() {
                Some((best, _))
                    if !cheaper(price, best.cost_per_second.unwrap_or(f64::INFINITY)) =>
                {
                    return false
                }
                _ => {
                    self.entries.clear();
                    self.entries.push((usage, item));
                    return true;
                }
            }
        }
        let point = front_point(&usage);
        if self
            .entries
            .iter()
            .any(|(kept, _)| dominates(&front_point(kept), &point))
        {
            return false;
        }
        self.entries
            .retain(|(kept, _)| !dominates(&point, &front_point(kept)));
        self.entries.push((usage, item));
        true
    }

    /// Every pair of `self`'s and `other`'s entries combined, two disjoint
    /// parts of a plan: additive usage summed, peak memory summed (Stage 3
    /// adds every node's peak), peak cores by maximum, each query's latency
    /// by maximum, the price summed (recomputed from the peaks under cost
    /// model 2); `join` combines the items. Kept as [`Self::insert`] keeps.
    pub(crate) fn combine<U: Clone, V: Clone>(
        &self,
        other: &Frontier<U>,
        join: impl Fn(&T, &U) -> V,
    ) -> Frontier<V> {
        let mut combined = Frontier::new(&self.cost_model);
        for (a, x) in &self.entries {
            for (b, y) in &other.entries {
                combined.insert(sum(a, b, &self.cost_model), join(x, y));
            }
        }
        combined
    }
}

/// `a` and `b` of two disjoint parts of a plan, combined ([`Frontier::combine`]).
fn sum(a: &LocalUsage, b: &LocalUsage, cost_model: &CostModel) -> LocalUsage {
    let mut latency_ms = a.latency_ms.clone();
    for &(query, latency) in &b.latency_ms {
        match latency_ms.iter_mut().find(|(q, _)| *q == query) {
            Some((_, l)) => *l = l.max(latency),
            None => latency_ms.push((query, latency)),
        }
    }
    latency_ms.sort_by_key(|(q, _)| *q);
    let mut usage = LocalUsage {
        cost_per_second: match (a.cost_per_second, b.cost_per_second) {
            (Some(x), Some(y)) => Some(x + y),
            _ => None,
        },
        cpu_ops_per_second: a.cpu_ops_per_second + b.cpu_ops_per_second,
        scanned_bytes_per_second: a.scanned_bytes_per_second + b.scanned_bytes_per_second,
        average_memory_bytes: a.average_memory_bytes + b.average_memory_bytes,
        peak_memory_bytes: a.peak_memory_bytes.saturating_add(b.peak_memory_bytes),
        peak_cpu_cores: a.peak_cpu_cores.max(b.peak_cpu_cores),
        disk_bytes: a.disk_bytes.saturating_add(b.disk_bytes),
        latency_ms,
    };
    usage.cost_per_second = provisioned_cost(&usage, cost_model);
    usage
}

fn over_limits(usage: &LocalUsage, limits: &ClusterLimits) -> bool {
    usage.peak_memory_bytes > limits.memory_bytes
        || usage.disk_bytes > limits.disk_bytes
        || limits
            .cpu_cores
            .is_some_and(|cores| usage.peak_cpu_cores > cores)
}

/// Stage 3's price comparison: `cost` is cheaper than `best` beyond its
/// tie tolerance (1e-9).
fn cheaper(cost: f64, best: f64) -> bool {
    cost < best - best.abs() * 1e-9
}

/// The Pareto front's coordinates: price (when there is one), peak memory,
/// peak cores, disk, worst latency.
fn front_point(usage: &LocalUsage) -> Vec<f64> {
    let mut point: Vec<f64> = usage.cost_per_second.into_iter().collect();
    point.extend([
        usage.peak_memory_bytes as f64,
        f64::from(usage.peak_cpu_cores),
        usage.disk_bytes as f64,
        usage.latency_ms.iter().map(|(_, l)| *l).fold(0.0, f64::max),
    ]);
    point
}

/// Whether `a` is no greater than `b` everywhere and smaller somewhere
/// (points of different shapes are incomparable).
fn dominates(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| x <= y)
        && a.iter().zip(b).any(|(x, y)| x < y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::subexpr::{CandidateOrigin, MemoGroup};
    use crate::{ClusterObjective, ResourceUnitCost};
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
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

    fn entries() -> Vec<QueryWorkloadEntry> {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        vec![entry_with(
            target,
            QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(60_000))),
        )]
    }

    /// A one-query workload's selected plan split into two groups, its root
    /// node over the rest, and the context of the selected
    /// materializations.
    fn split_plan(models: PlanningModels<'_>) -> (Memo, Context, Option<f64>) {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        let root = lower_promql("quantile_over_time(0.99, m[5m])", target);
        let run = crate::pass::run_stage1_to_stage3(
            vec![(0, QueryRoot::Operator(root))],
            &entries(),
            &data(),
            models,
        )
        .unwrap();
        let selection = run.stage3_plan_selection.as_ref().unwrap();
        let price = selection.costs[&selection.selected].cost_per_second;
        let assignment = run.stage3_selected.as_ref().unwrap().assignment.clone();
        let roots = run
            .stage1_selected
            .as_ref()
            .unwrap()
            .roots
            .as_ref()
            .unwrap();
        let QueryRoot::Operator(top) = &roots[0] else {
            panic!("an operator root")
        };
        let below = Rc::clone(top.children()[0]);
        let group = |node: &Rc<OperatorNode>, children: Vec<GroupId>| MemoGroup {
            node: Rc::clone(node),
            children,
            parents: Vec::new(),
            queries: vec![0],
            roots_of: Vec::new(),
        };
        let candidate =
            |id: u32, node: &Rc<OperatorNode>, children: Vec<GroupId>| ImplementationCandidate {
                implements: vec![GroupId(id)],
                node: Rc::clone(node),
                children,
                origin: CandidateOrigin::Pass1,
                coupling_sites: Vec::new(),
            };
        let memo = Memo {
            groups: vec![group(&below, vec![]), group(top, vec![GroupId(0)])],
            candidates: vec![
                vec![candidate(0, &below, vec![])],
                vec![candidate(1, top, vec![GroupId(0)])],
            ],
        };
        let context = Context {
            thresholds: Vec::new(),
            materialization: assignment.into_iter().map(|m| (GroupId(0), m)).collect(),
        };
        (memo, context, price)
    }

    #[test]
    fn local_prices_add_up_to_the_whole_plans_price() {
        let models = PlanningModels::builtin();
        let (memo, context, expected) = split_plan(models);
        let (entries, data) = (entries(), data());
        let mut cache =
            PriceCache::new(frozen_calibration(&memo, &entries, &data, models).unwrap());
        let mut total = 0.0;
        for (g, candidates) in memo.candidates.iter().enumerate() {
            let local = local_price(
                &memo,
                GroupId(g as u32),
                &candidates[0],
                &context,
                &[0],
                &entries,
                &data,
                models,
                &mut cache,
            )
            .unwrap()
            .unwrap();
            // Each group carries a part of the price, not the whole.
            let price = local.cost_per_second.unwrap();
            assert!(
                price > 0.0 && price < expected.unwrap(),
                "group {g}: {price}"
            );
            total += price;
        }
        let expected = expected.unwrap();
        assert!(
            (total - expected).abs() <= 1e-9 * expected.abs(),
            "{total} vs {expected}"
        );
    }

    #[test]
    fn the_cache_hits_on_a_repeated_context() {
        let models = PlanningModels::builtin();
        let (memo, context, _) = split_plan(models);
        let (entries, data) = (entries(), data());
        let mut cache = PriceCache::new(1.0);
        let mut price = || {
            local_price(
                &memo,
                GroupId(1),
                &memo.candidates[1][0],
                &context,
                &[0],
                &entries,
                &data,
                models,
                &mut cache,
            )
            .unwrap()
        };
        let first = price();
        let second = price();
        assert!(first.is_some());
        assert_eq!(first, second);
        assert_eq!((cache.misses, cache.hits), (1, 1));
    }

    fn usage(cost: f64, peak_memory_bytes: u64, peak_cpu_cores: u32, latency: f64) -> LocalUsage {
        LocalUsage {
            cost_per_second: Some(cost),
            cpu_ops_per_second: 1.0,
            scanned_bytes_per_second: 1.0,
            average_memory_bytes: 1.0,
            peak_memory_bytes,
            peak_cpu_cores,
            disk_bytes: 1,
            latency_ms: vec![(0, latency)],
        }
    }

    fn items<T: Clone>(frontier: &Frontier<T>) -> Vec<T> {
        frontier.entries.iter().map(|(_, t)| t.clone()).collect()
    }

    #[test]
    fn cost_model_1_keeps_the_cheapest_and_the_earlier_tie() {
        let mut frontier =
            Frontier::new(&CostModel::UsageOverTime(ResourceUnitCost::illustrative()));
        assert!(frontier.insert(usage(2.0, 1, 1, 1.0), "b"));
        assert!(frontier.insert(usage(1.0, 9, 9, 9.0), "a"));
        assert!(!frontier.insert(usage(1.0, 1, 1, 1.0), "tie"));
        assert!(!frontier.insert(usage(3.0, 0, 0, 0.0), "c"));
        assert_eq!(items(&frontier), vec!["a"]);
    }

    #[test]
    fn cost_model_2_keeps_the_pareto_front() {
        let mut frontier = Frontier::new(&CostModel::ProvisionedCapacity(
            ResourceUnitCost::illustrative(),
        ));
        assert!(frontier.insert(usage(2.0, 10, 1, 5.0), "cheap memory"));
        assert!(frontier.insert(usage(1.0, 20, 1, 5.0), "cheap price"));
        // Dominated by "cheap price".
        assert!(!frontier.insert(usage(1.0, 20, 1, 6.0), "slower"));
        // An exact tie is kept.
        assert!(frontier.insert(usage(1.0, 20, 1, 5.0), "tie"));
        // Dominates "cheap memory".
        assert!(frontier.insert(usage(2.0, 10, 1, 4.0), "faster"));
        assert_eq!(items(&frontier), vec!["cheap price", "tie", "faster"]);
    }

    #[test]
    fn combining_sums_usage_and_takes_the_largest_peak_cores_and_latency() {
        let cluster = CostModel::ClusterCapacity(ClusterLimits {
            memory_bytes: 25,
            disk_bytes: u64::MAX,
            cpu_cores: Some(4),
            objective: ClusterObjective::ParetoFront,
        });
        let mut a = Frontier::new(&cluster);
        let none = |mut u: LocalUsage| {
            u.cost_per_second = None;
            u
        };
        a.insert(none(usage(0.0, 10, 2, 5.0)), 1);
        a.insert(none(usage(0.0, 5, 3, 7.0)), 2);
        let mut b = Frontier::new(&cluster);
        b.insert(none(usage(0.0, 12, 4, 6.0)), 10);
        let combined = a.combine(&b, |x, y| x + y);
        // 10 + 12 and 5 + 12 bytes both fit under 25.
        assert_eq!(items(&combined), vec![11, 12]);
        let (first, _) = &combined.entries[0];
        assert_eq!(first.peak_memory_bytes, 22);
        assert_eq!(first.peak_cpu_cores, 4);
        assert_eq!(first.latency_ms, vec![(0, 6.0)]);
        assert_eq!(first.cpu_ops_per_second, 2.0);
        assert_eq!(first.disk_bytes, 2);
        // Over the memory limit: dropped.
        let mut c = Frontier::new(&cluster);
        c.insert(none(usage(0.0, 21, 1, 1.0)), 100);
        assert!(a.combine(&c, |x, y| x + y).entries.is_empty());
    }
}
