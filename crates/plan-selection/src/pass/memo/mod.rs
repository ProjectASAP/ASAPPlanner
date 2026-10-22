//! Memo-group search (joint-planning-search design, "Memo-group search"):
//! each memo group of queries enumerates and prices its own candidates
//! once, keeps the cheapest per sharing signature, and only groups whose
//! sharing sites can meet make a joint choice.
//!
//! Not wired into the facade yet: it runs beside the current search until
//! it selects the same price on every equivalence test.
//!
//! - [`groups`]: steps 1–2, the memo groups and their candidates with
//!   their signatures;
//! - [`pricing`]: steps 2–3, each candidate priced alone (cached) and the
//!   dominated ones per signature dropped;
//! - [`clusters`]: steps 4–6, the sharing clusters and the exact joint
//!   choice in each.
#![allow(dead_code)]

pub(crate) mod clusters;
pub(crate) mod derivations;
pub(crate) mod groups;
pub(crate) mod pricing;
pub(crate) mod sharing;

use asap_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::QueryRoot;
use asap_types::physical::Materialization;

/// One memo group: queries planned together as one unit. One query, every
/// query of a class of interchangeable queries, or queries that read a
/// common Pass 1 target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoGroup {
    /// Its queries, by index in the workload, in workload order.
    pub queries: Vec<usize>,
    /// The Pass 1 targets its queries read: digits of a whole-workload
    /// choice, in increasing order.
    pub targets: Vec<usize>,
}

/// One candidate of a memo group, planned alone.
#[derive(Debug, Clone)]
pub(crate) struct GroupCandidate {
    /// Its group, by index.
    pub group: usize,
    /// The group's Pass 1 choice: one candidate per target, in the order
    /// of [`MemoGroup::targets`].
    pub choice: Vec<usize>,
    /// The group's roots, in its queries' order, with the Pass 2 rewrites
    /// that touch only the group's nodes.
    pub roots: Vec<QueryRoot>,
    /// One materialization per build only this group reads, in Stage 2's
    /// discovery order.
    pub materialization: Vec<Materialization>,
    /// The sites another group could share.
    pub signature: SharingSignature,
    /// The group's usage planned alone; `None` until priced.
    pub usage: Option<LocalUsage>,
}

/// The sharing sites a group candidate exposes, sorted: candidates of one
/// group with equal signatures compete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SharingSignature(pub Vec<SharingSite>);

/// A node another group could read once a Pass 2 rule makes it one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SharingSite {
    /// Equal keys meet: a hash of the node's input and computation with
    /// sizes and quantile ranks masked.
    pub meeting_key: u64,
    /// A hash of what decides the shared node's cost: family and size,
    /// pane grid, materialization.
    pub cost_key: u64,
}

/// One group candidate's usage, priced by Stage 3 with the group alone.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LocalUsage {
    pub cpu_ops_per_second: f64,
    pub scanned_bytes_per_second: f64,
    pub average_memory_bytes: f64,
    pub peak_memory_bytes: u64,
    pub disk_bytes: u64,
    /// Each of the group's queries' latency per evaluation, in ms, in the
    /// order of [`MemoGroup::queries`].
    pub latency_ms: Vec<f64>,
    /// The price per second under the deployment's cost model, when it has
    /// one (cost models 1 and 2).
    pub cost_per_second: Option<f64>,
}

/// One cluster's selection: a kept candidate per group, and the plan they
/// compose.
#[derive(Debug, Clone)]
pub(crate) struct ClusterSelection {
    /// Its groups, by index.
    pub groups: Vec<usize>,
    /// Per group of [`Self::groups`], the index of its selected candidate.
    pub selected: Vec<usize>,
    /// The cluster's queries, by index in the workload, in workload order.
    pub queries: Vec<usize>,
    /// Their logical roots, in the order of [`Self::queries`]: what Stage 2
    /// implemented.
    pub roots: Vec<QueryRoot>,
    /// The cluster's physical candidate, as Stage 2 built it, priced by
    /// Stage 3.
    pub physical: PhysicalCandidate,
}

/// The memo-group search's selection over the workload of
/// `per_computation` (the Pass 1 inventory, already pruned for the
/// deployment's capabilities): the selection Stage 3 makes of the composed
/// plan, the plan as Stage 2 built it, and its logical roots. `None` when
/// its preconditions do not hold (only cost model 1 without a measured cost
/// table or a disk price); the caller then runs the current search.
pub(crate) fn search<Id: Clone>(
    per_computation: &asap_logical_asap_optimization::per_computation_candidates::PerComputationCandidates<Id>,
    entries: &[asap_types::workload::QueryWorkloadEntry],
    data: &asap_types::workload::DataWorkload,
    models: crate::PlanningModels<'_>,
) -> Option<Result<(crate::Selection, PhysicalCandidate, Vec<QueryRoot>), crate::SelectionError>> {
    let crate::CostModel::UsageOverTime(prices) = &models.plan_selection.cost_model else {
        return None;
    };
    if prices.cost_per_disk_byte_second != 0.0
        || !models
            .plan_selection
            .measured_operation_costs
            .operators
            .is_empty()
    {
        return None;
    }
    let memo = groups::memo_groups(per_computation, entries, &models);
    let mut cache = pricing::LocalPriceCache::default();
    let mut kept = Vec::with_capacity(memo.groups.len());
    for (g, group) in memo.groups.iter().enumerate() {
        let candidates = groups::group_candidates(per_computation, &memo, g, entries, data, models);
        let priced = match pricing::priced(group, candidates, entries, data, models, &mut cache) {
            Ok(priced) => priced,
            Err(error) => return Some(Err(error)),
        };
        kept.push(pricing::keep_undominated(priced));
    }
    let selections = match clusters::select_clusters(&memo, &kept, entries, data, models) {
        Ok(selections) => selections,
        Err(error) => return Some(Err(error)),
    };
    let mut roots: Vec<Option<QueryRoot>> = vec![None; entries.len()];
    for selection in &selections {
        for (&q, root) in selection.queries.iter().zip(&selection.roots) {
            roots[q] = Some(root.clone());
        }
    }
    let Some(roots) = roots.into_iter().collect::<Option<Vec<_>>>() else {
        return Some(Err(crate::SelectionError::NoValidCandidate(Vec::new())));
    };
    let Some(mut physical) = clusters::composed(&selections, entries.len(), entries, data, models)
    else {
        return Some(Err(crate::SelectionError::NoValidCandidate(Vec::new())));
    };
    physical.id = "M1".to_string();
    physical.from_logical = "L1".to_string();
    let finished =
        crate::PlanSelector::new(entries, data, models, false).and_then(|mut selector| {
            selector.offer(&physical)?;
            selector.finish()
        });
    Some(finished.map(|(mut selection, selected)| {
        selection.search.candidates_checked += cache.misses + cache.hits;
        (selection, selected, roots)
    }))
}
