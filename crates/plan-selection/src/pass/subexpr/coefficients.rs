//! Each group's choices and their coefficients for the MILP ([`super::milp`]),
//! from Stage 3's per-node pricing.
//!
//! A choice is one candidate of a group with an option for each of its
//! private summary builds (its slots), as Stage 2 offers them for the
//! group's queries, down-closure applied. Its candidate's sub-DAG is built
//! by Stage 2 once per assignment, one root per query of the group, and
//! every node is measured for one computation ([`crate::price_node`] at
//! rate `(1, 1.0)`). Its private nodes (those not in a child group's own
//! sub-DAG, by structure) are split into parts by who computes them, as
//! Stage 3's `computations` counts consumers:
//!
//! - the direct part: reached from the candidate's node without passing a
//!   materialized slot, computed once per consumer reaching the group;
//! - one part per materialized slot: the slot and what it reaches down to
//!   the next materialized slot or a child, computed by the slot.
//!
//! Where each field comes from (all of [`crate::price_node`]'s outputs for
//! one computation, cost model 1's [`crate::ResourceUnitCost`]):
//!
//! | Field | Source |
//! | --- | --- |
//! | `cost_per_unit_rate` | `cpu_ops_per_second · cost_per_cpu_op + scanned_bytes_per_second · cost_per_scanned_byte` |
//! | `fixed_cost` | at ingestion: the per-computation price at rate 1, plus kept state; at query time: kept state only. Kept state is `average_memory_bytes · cost_per_memory_byte_second + disk_bytes · cost_per_disk_byte_second`, a pane chain's from `retained_panes` over `pane_retention_ms` with the group's queries' range spans |
//! | `work_ms` | `work_ms` of the part's query-time nodes (`query_latencies` adds each reached node once) |
//! | [`LazyTerms::held`] | `held`: bytes held while an evaluation runs, charged by Stage 3 as `bytes · per_second · E / 1000 · cost_per_memory_byte_second`, `E` the readers' rate-weighted mean latency (per reader) or longest (built once) |
//! | [`LazyTerms::raw_scans`] | `raw_scan`: Stage 3 keeps each source's longest query-time scan on disk |
//!
//! Limits: a child's output statistics are its group's own node's, as the
//! candidate's sub-DAG reads it, whatever the child's chosen
//! implementation; a query's request overhead (a measured table's
//! `ns_per_evaluation`) and an ingestion state's compaction and partial
//! copies with more than one ingest worker are not coefficients; a kept
//! chain's slot is charged per unit of its group's union rate, while
//! Stage 3 computes it once per request (range steps not counted).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::milp::{
    Choice, ChoiceCoefficients, PartCoefficients, SharedNodeCoefficients, SlotCoefficients, Via,
};
use super::pricing::{child_keys, keys_of, scan_lookback_ms, PriceCache};
use super::{CouplingKind, CouplingSite, GroupId, ImplementationCandidate, Memo};
use crate::{
    kept_pane_chains, maintained_window_ms, node_map, pane_retention_ms, price_node, query_reach,
    retained_panes, source_name, windows_read, CostModel, PlanningModels, PricedNode,
    ProducerOutput, SelectionError,
};

/// The terms of a choice the MILP adds lazily, once its plan's latencies
/// and every source's scans are known.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LazyTerms {
    /// Memory each part holds while an evaluation runs.
    pub held: Vec<HeldTerm>,
    /// Each query-time raw scan of a part: source, bytes, part.
    pub raw_scans: Vec<(String, u64, Via)>,
    /// The direct part computed at ingestion time, as it is under a
    /// parent's ingestion-time slot (scans read one second of rows);
    /// `None` when it cannot run at ingestion time (a grouping with no
    /// window below it).
    pub direct_at_ingestion: Option<PartCoefficients>,
    /// The threshold of each coupling site the choice's candidate reads it
    /// at (index among the site's thresholds); a raw-horizon site's is
    /// Stage 3's own.
    pub thresholds: Vec<(CouplingSite, usize)>,
    /// When the group's own node is built once at query time (in memory):
    /// the state it holds while its longest reader runs, charged in place
    /// of its [`HeldTerm::own_node`] working memory.
    pub built_once_state_bytes: u64,
}

/// Bytes a part holds while an evaluation runs: Stage 3 charges
/// `bytes · rate · E / 1000` byte-seconds per second, `rate` the part's
/// consumers' rate and `E` the queries reaching it: their rate-weighted
/// mean latency when each computes its own copy (`per_reader`), else the
/// longest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HeldTerm {
    pub via: Via,
    pub bytes: u64,
    pub per_reader: bool,
    /// Whether it is the group's own node (replaced when it is built once).
    pub own_node: bool,
    /// Its node's shallow key ([`shallow_keys`]).
    pub node_key: u32,
}

/// Every choice of `group` with its lazy terms, in candidate order and,
/// within a candidate, in Stage 2's assignment order.
pub(crate) fn choices(
    memo: &Memo,
    group: GroupId,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<(Vec<Choice>, Vec<LazyTerms>), SelectionError> {
    let parts = choice_parts(memo, group, entries, data, models, cache)?;
    Ok(parts
        .into_iter()
        .map(|part| (part.choice, part.lazy))
        .unzip())
}

/// The threshold of each coupling site a candidate is read at (index among
/// the site's thresholds).
type SiteThresholds = Vec<(CouplingSite, usize)>;

/// A choice with the shallow keys ([`shallow_keys`]) of its nodes, by
/// which a composed plan's nodes are found (tests).
pub(super) struct ChoiceParts {
    pub choice: Choice,
    pub lazy: LazyTerms,
    /// The shallow key of the candidate's node.
    pub root_key: u32,
    /// The shallow keys of the candidate node's private producers.
    pub root_producer_keys: Vec<u32>,
    /// Per shared node of the coefficients, the shallow keys of the nodes
    /// it computes when built once, itself first.
    pub shared_node_keys: Vec<Vec<u32>>,
    /// Per slot of the coefficients, the shallow key of its build.
    pub slot_keys: Vec<u32>,
    /// The shallow keys of the private builds, in assignment order.
    pub build_keys: Vec<u32>,
    /// Per part (direct, then each slot), its nodes' keys.
    pub part_keys: Vec<HashSet<u32>>,
}

pub(super) fn choice_parts(
    memo: &Memo,
    group: GroupId,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: PlanningModels<'_>,
    cache: &mut PriceCache,
) -> Result<Vec<ChoiceParts>, SelectionError> {
    let node = &memo.groups[group.0 as usize].node;
    let queries = &memo.groups[group.0 as usize].queries;
    let windows_of_raw = Rc::clone(
        cache
            .raw_windows
            .get_or_insert_with(|| Rc::new(raw_windows(memo))),
    );
    let is_scan = matches!(node.non_asap(), Some(NonASAPOp::Scan { .. }));
    // Shared (one node for every query), and for a group several queries
    // read, apart (a copy per query); a scan apart is its reader's.
    let mut modes: Vec<Option<usize>> = vec![None];
    if queries.len() > 1 && !is_scan {
        modes.extend(queries.iter().map(|&q| Some(q)));
    }
    // A shared scan reads the longest window of its readers: one choice
    // per horizon a reader may read.
    let own_horizons: Vec<Option<u64>> = match node.non_asap() {
        Some(NonASAPOp::Scan { source, .. }) => windows_of_raw
            .get(&super::scans::raw_horizon_key(source))
            .map(|windows| windows.iter().map(|&w| Some(w)).collect())
            .unwrap_or_else(|| vec![None]),
        _ => vec![None],
    };
    let mut out = Vec::new();
    let versions: Vec<(usize, ImplementationCandidate, SiteThresholds)> = memo.candidates
        [group.0 as usize]
        .iter()
        .enumerate()
        .flat_map(|(index, candidate)| {
            at_site_thresholds(memo, candidate)
                .into_iter()
                .map(move |(version, thresholds)| (index, version, thresholds))
        })
        .collect();
    for copy_of in modes {
        // A copy for one query is built for it twice, so that Stage 2
        // offers its builds built once for several readers (`MatAtQuery`):
        // another group's or copy's identical build may share it.
        let built_for: Vec<usize> = match (copy_of, queries.as_slice()) {
            (Some(q), _) | (None, &[q]) => vec![q, q],
            (None, _) => queries.clone(),
        };
        let part_entries: Vec<QueryWorkloadEntry> =
            built_for.iter().map(|&q| entries[q].clone()).collect();
        for (index, candidate, thresholds) in &versions {
            let (index, candidate) = (*index, candidate);
            let roots =
                vec![QueryRoot::Operator(Rc::clone(&candidate.node)); built_for.len().max(1)];
            let Ok(physicals) = optimize_physical_asap(
                "S",
                &roots,
                &part_entries,
                data,
                models.deployment_capabilities,
            ) else {
                continue;
            };
            let child_groups = children_of(memo, group, candidate);
            let child_key_sets: Vec<Rc<super::pricing::ChildKeys>> = child_groups
                .iter()
                .map(|&child| child_keys(memo, child, entries, data, &models, cache))
                .collect();
            // Each scan child: apart (its nodes this choice's) or shared at
            // a horizon no shorter than this choice's window on it.
            let scan_children: Vec<(usize, u64, Vec<Option<u64>>)> = child_groups
                .iter()
                .enumerate()
                .filter_map(
                    |(i, child)| match memo.groups[child.0 as usize].node.non_asap() {
                        Some(NonASAPOp::Scan { source, .. }) => {
                            let key = super::scans::raw_horizon_key(source);
                            let mut variants = vec![None];
                            variants.extend(
                                windows_of_raw
                                    .get(&key)
                                    .into_iter()
                                    .flatten()
                                    .map(|&w| Some(w)),
                            );
                            Some((i, key, variants))
                        }
                        _ => None,
                    },
                )
                .collect();
            let mut combinations: Vec<Vec<Option<u64>>> = vec![Vec::new()];
            for (_, _, variants) in &scan_children {
                combinations = combinations
                    .into_iter()
                    .flat_map(|chosen| {
                        variants.iter().map(move |v| {
                            let mut chosen = chosen.clone();
                            chosen.push(*v);
                            chosen
                        })
                    })
                    .collect();
            }
            for own_horizon in &own_horizons {
                for combination in &combinations {
                    // Children whose nodes are not this choice's, and the
                    // horizon each shared scan child reads.
                    let mut horizons: HashMap<u64, u64> = HashMap::new();
                    let mut absorbed: Vec<usize> = Vec::new();
                    for ((i, key, _), variant) in scan_children.iter().zip(combination) {
                        match variant {
                            Some(h) => {
                                horizons.insert(*key, *h);
                            }
                            None => absorbed.push(*i),
                        }
                    }
                    let mut children: HashSet<u32> = HashSet::new();
                    let mut child_roots: Vec<Option<u32>> = Vec::new();
                    let mut kept_children: Vec<GroupId> = Vec::new();
                    for (i, keys) in child_key_sets.iter().enumerate() {
                        if absorbed.contains(&i) {
                            continue;
                        }
                        children.extend(keys.nodes.iter().copied());
                        child_roots.push(keys.root);
                        kept_children.push(child_groups[i]);
                    }
                    let mut seen: Vec<Vec<Materialization>> = Vec::new();
                    for physical in &physicals {
                        // A shared scan's horizon is the longest window of
                        // its readers: no shorter than this choice's own
                        // window on it (through time shifts).
                        if !horizons.is_empty() {
                            let dag = &physical.dag;
                            let nodes = node_map(dag);
                            let kept = kept_pane_chains(dag);
                            let too_short = dag.nodes.iter().any(|n| match &n.payload {
                                Operator::NonASAP(NonASAPOp::Scan { source, .. }) => horizons
                                    .get(&super::scans::raw_horizon_key(source))
                                    .is_some_and(|&h| {
                                        windows_read(dag, &nodes, n.id, &kept)
                                            .into_iter()
                                            .any(|w| w > h)
                                    }),
                                _ => false,
                            });
                            if too_short {
                                continue;
                            }
                        }
                        let keys = keys_of(&physical.dag, cache.keys_mut());
                        let shallow = shallow_keys(&physical.dag, cache.keys_mut());
                        let private: Vec<bool> =
                            keys.iter().map(|k| !children.contains(k)).collect();
                        let builds: Vec<usize> = (0..physical.dag.nodes.len())
                            .filter(|&i| private[i] && is_build(&physical.dag.nodes[i].payload))
                            .collect();
                        // Only the private builds are the choice's; any
                        // other materialized node (the group's own node
                        // built once, a child's build) is another variable
                        // of the model.
                        let foreign = physical.dag.nodes.iter().enumerate().any(|(i, n)| {
                            n.materialization != Materialization::NotMaterialized
                                && !builds.contains(&i)
                        });
                        if foreign {
                            continue;
                        }
                        let assignment: Vec<Materialization> = builds
                            .iter()
                            .map(|&i| physical.dag.nodes[i].materialization)
                            .collect();
                        if seen.contains(&assignment) {
                            continue;
                        }
                        seen.push(assignment.clone());
                        let Some(parts) = coefficients_of(
                            physical,
                            &keys,
                            &shallow,
                            &private,
                            &child_roots,
                            &horizons,
                            own_horizon.or_else(|| scan_lookback_ms(memo, group)),
                            &part_entries,
                            data,
                            &models,
                        )?
                        else {
                            continue;
                        };
                        out.push(ChoiceParts {
                            choice: Choice {
                                group,
                                candidate: index,
                                implements: candidate.implements.clone(),
                                children: kept_children.clone(),
                                assignment,
                                coefficients: parts.coefficients,
                                copy_of,
                                scan_children: scan_children
                                    .iter()
                                    .zip(combination)
                                    .map(|((i, _, _), v)| (child_groups[*i], *v))
                                    .collect(),
                                scan_horizon_ms: *own_horizon,
                                scan_source: match node.non_asap() {
                                    Some(NonASAPOp::Scan { source, .. }) => {
                                        Some(super::scans::raw_horizon_key(source))
                                    }
                                    _ => None,
                                },
                                scan_project_cost_per_unit_rate: parts.scan_project_cost,
                            },
                            lazy: LazyTerms {
                                thresholds: thresholds.clone(),
                                ..parts.lazy
                            },
                            root_key: parts.root_key,
                            root_producer_keys: parts.root_producer_keys,
                            shared_node_keys: parts.shared_node_keys,
                            slot_keys: parts.slot_keys,
                            build_keys: builds.iter().map(|&i| shallow[i]).collect(),
                            part_keys: parts.part_keys,
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// `candidate` rewritten at each combination of thresholds of its coupling
/// sites that it may read, as the brute-force search rewrites a plan's
/// candidates, with the index of each threshold. A raw-horizon site has
/// no threshold here: scan sharing is part of each choice
/// ([`Choice::scan_children`]). A candidate without sites is itself.
fn at_site_thresholds(
    memo: &Memo,
    candidate: &ImplementationCandidate,
) -> Vec<(ImplementationCandidate, Vec<(CouplingSite, usize)>)> {
    use super::{capability, scans, windows};
    let mut versions = vec![(candidate.clone(), Vec::new())];
    for site in &candidate.coupling_sites {
        let mut next = Vec::new();
        for (version, chosen) in versions {
            let at = |index: usize, rewritten: Option<ImplementationCandidate>| {
                rewritten.map(|rewritten| {
                    let mut chosen = chosen.clone();
                    chosen.push((*site, index));
                    (rewritten, chosen)
                })
            };
            match site.kind {
                CouplingKind::PaneChain => {
                    for (index, threshold) in
                        windows::chain_thresholds(memo, site).iter().enumerate()
                    {
                        let readers_ok = windows::chain_readers(&version.node)
                            .iter()
                            .filter(|r| r.key == site.key)
                            .all(|r| windows::may_read(threshold, r));
                        let rewritten = readers_ok
                            .then(|| windows::read_at_threshold(&version, site, threshold).ok())
                            .flatten()
                            .map(|node| ImplementationCandidate {
                                node,
                                ..version.clone()
                            });
                        next.extend(at(index, rewritten));
                    }
                }
                CouplingKind::ScanLabels => {
                    for (index, threshold) in scans::label_thresholds(memo, site).iter().enumerate()
                    {
                        let rewritten =
                            scans::widened_at_threshold(memo, &version, site, threshold)
                                .ok()
                                .flatten();
                        next.extend(at(index, rewritten));
                    }
                }
                // A sketch rollup's finer build, sized for its readers, as
                // the brute force sizes it.
                CouplingKind::SharedBuildSize if windows::is_rollup_state(memo, site) => {
                    for (index, threshold) in windows::state_size_thresholds(memo, site)
                        .iter()
                        .enumerate()
                    {
                        let rewritten = windows::may_read_state(threshold, &version, site)
                            .then(|| windows::resize_state_at(&version, site, threshold).ok())
                            .flatten()
                            .map(|node| ImplementationCandidate {
                                node,
                                ..version.clone()
                            });
                        next.extend(at(index, rewritten));
                    }
                }
                CouplingKind::SharedBuildSize => {
                    for (index, threshold) in
                        capability::size_thresholds(memo, site).iter().enumerate()
                    {
                        let rewritten =
                            capability::resized_at_threshold(memo, &version, site, threshold)
                                .ok()
                                .flatten()
                                .map(|node| ImplementationCandidate {
                                    node,
                                    ..version.clone()
                                });
                        next.extend(at(index, rewritten));
                    }
                }
                // A raw horizon is the look-back of a shared scan: each
                // choice prices its scan children shared at a horizon or
                // apart ([`Choice::scan_children`]).
                CouplingKind::RawHorizon => next.push((version, chosen)),
            }
        }
        versions = next;
    }
    versions
}

/// Every window a raw scan of each source may be read for, by the source's
/// raw-horizon key: each range over it (through time shifts), and each
/// pane chain over it, kept across evaluations (its pane width) or not
/// (its panes), in every candidate and every chain threshold of the memo.
fn raw_windows(memo: &Memo) -> HashMap<u64, BTreeSet<u64>> {
    use asap_types::ir::OperatorNode;
    let mut windows: HashMap<u64, BTreeSet<u64>> = HashMap::new();
    let scan_below = |node: &Rc<OperatorNode>| -> Option<u64> {
        let mut node = Rc::clone(node);
        loop {
            match node.non_asap() {
                Some(NonASAPOp::Scan { source, .. }) => {
                    return Some(super::scans::raw_horizon_key(source))
                }
                Some(NonASAPOp::TimeShift { child, .. }) => node = Rc::clone(child),
                _ => return None,
            }
        }
    };
    for candidate in memo.candidates.iter().flatten() {
        for (version, _) in at_site_thresholds(memo, candidate) {
            for node in OperatorNode::reachable(&version.node) {
                match (node.non_asap(), node.asap()) {
                    (Some(NonASAPOp::TimeRange { range, child, .. }), _) => {
                        if let Some(key) = scan_below(child) {
                            let ms = u64::try_from(range.as_millis()).unwrap_or(u64::MAX);
                            windows.entry(key).or_default().insert(ms);
                        }
                    }
                    (
                        _,
                        Some(ASAPOp::PaneChain {
                            child,
                            pane_width_ms,
                            pane_count,
                            ..
                        }),
                    ) => {
                        if let Some(key) = scan_below(child) {
                            let entry = windows.entry(key).or_default();
                            entry.insert(*pane_width_ms);
                            entry.insert(pane_width_ms.saturating_mul(u64::from(*pane_count)));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    windows
}

/// The child groups of `candidate`: its own, and every other group whose
/// node equals a node its node reaches and that no other child already
/// reaches, which some Pass 2 candidates read without listing (a pane
/// chain over its window's scan). Their nodes are not the candidate's
/// (the composed plan makes equal nodes one), and a plan choosing it
/// needs them.
fn children_of(memo: &Memo, group: GroupId, candidate: &ImplementationCandidate) -> Vec<GroupId> {
    use asap_types::ir::OperatorNode;
    let mut children = candidate.children.clone();
    let reachable = OperatorNode::reachable(&candidate.node);
    let reaches = |nodes: &[Rc<OperatorNode>], g: GroupId| {
        let node = &memo.groups[g.0 as usize].node;
        nodes.iter().any(|n| {
            Rc::ptr_eq(n, node) || (n.operator == node.operator && n.schema == node.schema)
        })
    };
    let below = |g: GroupId, children: &[GroupId]| {
        children
            .iter()
            .any(|&c| reaches(&OperatorNode::reachable(&memo.groups[c.0 as usize].node), g))
    };
    // Parents before children, so the highest group read is taken first.
    for h in (0..memo.groups.len()).rev() {
        let h = GroupId(h as u32);
        if h == group || candidate.implements.contains(&h) || children.contains(&h) {
            continue;
        }
        if !Rc::ptr_eq(&memo.groups[h.0 as usize].node, &candidate.node)
            && memo.groups[h.0 as usize].node.operator != candidate.node.operator
            && reaches(&reachable, h)
            && !below(h, &children)
        {
            children.push(h);
        }
    }
    children
}

fn is_build(payload: &Operator<PhysicalASAPNodeId>) -> bool {
    matches!(
        payload,
        Operator::ASAP(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
    )
}

struct Coefficients {
    coefficients: ChoiceCoefficients,
    /// For a scan: what a pass over its output costs per computation.
    scan_project_cost: f64,
    lazy: LazyTerms,
    root_key: u32,
    root_producer_keys: Vec<u32>,
    shared_node_keys: Vec<Vec<u32>>,
    slot_keys: Vec<u32>,
    part_keys: Vec<HashSet<u32>>,
}

/// Every node of `dag` measured for one computation, children first.
fn per_computation<'a>(
    dag: &'a PhysicalASAPDAG,
    entries: &[QueryWorkloadEntry],
    scan_lookback: Option<u64>,
    horizons: &HashMap<u64, u64>,
    data: &DataWorkload,
    models: &'a PlanningModels<'a>,
) -> Result<HashMap<PhysicalASAPNodeId, PricedNode<'a>>, SelectionError> {
    let nodes = node_map(dag);
    let kept = kept_pane_chains(dag);
    let reach = query_reach(dag, &nodes);
    let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
    let retention = pane_retention_ms(dag, &reach, &ranges);
    let mut producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for e in &dag.edges {
        producers_of.entry(e.consumer).or_default().push(e.producer);
    }
    let mut priced: HashMap<PhysicalASAPNodeId, PricedNode<'a>> = HashMap::new();
    for node in &dag.nodes {
        let views: Vec<ProducerOutput<'a>> = producers_of
            .get(&node.id)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .map(|p| {
                let produced = &priced[p];
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
        let lookback_ms = match (&node.payload, node.output_state.timing) {
            (Operator::NonASAP(NonASAPOp::Scan { source, .. }), ExecutionTiming::QueryTime) => {
                match horizons.get(&super::scans::raw_horizon_key(source)) {
                    Some(&horizon) => Some(horizon),
                    None => windows_read(dag, &nodes, node.id, &kept)
                        .into_iter()
                        .max()
                        .or(scan_lookback),
                }
            }
            _ => None,
        };
        let node_priced = price_node(
            node,
            &views,
            (1, 1.0),
            lookback_ms,
            retained_panes(node, &retention),
            (node.output_state.timing == ExecutionTiming::IngestionTime)
                .then(|| maintained_window_ms(node.id, &nodes, &producers_of))
                .flatten(),
            data,
            models.plan_selection,
            1.0,
        )
        .map_err(|error| SelectionError::Cost {
            candidate: "memo choice".into(),
            node: node.id,
            error,
        })?;
        priced.insert(node.id, node_priced);
    }
    Ok(priced)
}

/// Each node's shallow key, by position: its payload with its producers
/// left out, and its output schema. Unlike [`keys_of`] it does not depend
/// on the implementations below a node.
pub(super) fn shallow_keys(dag: &PhysicalASAPDAG, interned: &mut HashMap<String, u32>) -> Vec<u32> {
    dag.nodes
        .iter()
        .map(|node| {
            let payload = node.payload.map_children(|_| ());
            let text = serde_json::to_string(&("shallow", &payload, &node.output_schema))
                .expect("a physical node serializes");
            let next = interned.len() as u32;
            *interned.entry(text).or_insert(next)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn coefficients_of(
    physical: &PhysicalCandidate,
    keys: &[u32],
    shallow: &[u32],
    private: &[bool],
    child_roots: &[Option<u32>],
    horizons: &HashMap<u64, u64>,
    scan_lookback: Option<u64>,
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    models: &PlanningModels<'_>,
) -> Result<Option<Coefficients>, SelectionError> {
    let CostModel::UsageOverTime(prices) = &models.plan_selection.cost_model else {
        return Ok(None);
    };
    let dag = &physical.dag;
    let Some(&root) = dag.roots.first() else {
        return Ok(None);
    };
    let position: HashMap<PhysicalASAPNodeId, usize> = dag
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id, i))
        .collect();
    if !private[position[&root]] {
        return Ok(None);
    }
    let priced = per_computation(dag, entries, scan_lookback, horizons, data, models)?;
    let mut producers_of: HashMap<PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>> = HashMap::new();
    for e in &dag.edges {
        producers_of.entry(e.consumer).or_default().push(e.producer);
    }
    let is_private = |id: PhysicalASAPNodeId| private[position[&id]];
    let materialized = |id: PhysicalASAPNodeId| {
        dag.nodes[position[&id]].materialization != Materialization::NotMaterialized
    };
    // A part's nodes: from `start`, private nodes, not past a materialized
    // one (other than `start`); and the non-private nodes it reads.
    let walk = |start: PhysicalASAPNodeId| {
        let mut part = Vec::new();
        let mut reads = Vec::new();
        let mut seen = HashSet::new();
        let mut stack = vec![start];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if !is_private(id) {
                reads.push(id);
                continue;
            }
            if id != start && materialized(id) {
                continue;
            }
            part.push(id);
            stack.extend(producers_of.get(&id).into_iter().flatten().copied());
        }
        (part, reads)
    };
    let per_rate = |id: PhysicalASAPNodeId| {
        let p = &priced[&id];
        p.cpu_ops_per_second * prices.cost_per_cpu_op
            + p.scanned_bytes_per_second * prices.cost_per_scanned_byte
    };
    let kept_state = |id: PhysicalASAPNodeId| {
        let p = &priced[&id];
        p.average_memory_bytes * prices.cost_per_memory_byte_second
            + p.disk_bytes as f64 * prices.cost_per_disk_byte_second
    };
    let query_time = |id: PhysicalASAPNodeId| {
        dag.nodes[position[&id]].output_state.timing == ExecutionTiming::QueryTime
    };
    let work = |part: &[PhysicalASAPNodeId]| -> f64 {
        part.iter()
            .filter(|&&id| query_time(id))
            .map(|id| priced[id].work_ms)
            .sum()
    };
    let mut held = Vec::new();
    let mut raw_scans = Vec::new();
    let mut lazy_of = |part: &[PhysicalASAPNodeId], via: Via| {
        for &id in part {
            let p = &priced[&id];
            if let Some((bytes, _, per_reader)) = p.held {
                held.push(HeldTerm {
                    via,
                    bytes,
                    per_reader,
                    own_node: id == root,
                    node_key: shallow[position[&id]],
                });
            }
            if let Some((source, bytes)) = p.raw_scan {
                raw_scans.push((source_name(source).to_string(), bytes, via));
            }
        }
    };
    let (direct, direct_reads) = walk(root);
    lazy_of(&direct, Via::Direct);
    let direct_coefficients = PartCoefficients {
        cost_per_unit_rate: direct.iter().map(|&id| per_rate(id) + kept_state(id)).sum(),
        work_ms: work(&direct),
    };
    // Shareable nodes of the direct part (another copy's or group's
    // identical node may be one with it), each with what it computes when
    // built once: itself and the direct nodes only it reaches.
    let mut shared_nodes = Vec::new();
    let mut shared_node_keys = Vec::new();
    let direct_set: HashSet<PhysicalASAPNodeId> = direct.iter().copied().collect();
    // Each shareable node with the direct nodes below it.
    let shareable: Vec<(PhysicalASAPNodeId, Vec<PhysicalASAPNodeId>)> = direct
        .iter()
        .copied()
        .filter(|&id| {
            let node = &dag.nodes[position[&id]];
            let shareable = matches!(node.payload, Operator::ASAP(_))
                || !node.output_schema.unique_keys.is_empty();
            id != root && shareable && !is_build(&node.payload) && query_time(id)
        })
        .map(|id| {
            let below = walk(id)
                .0
                .into_iter()
                .filter(|n| direct_set.contains(n))
                .collect();
            (id, below)
        })
        .collect();
    // Each direct node belongs to its nearest shareable node (itself or
    // above it), so that no node is in two of them: the model charges
    // their sum once.
    for (id, below) in &shareable {
        let id = *id;
        let below: Vec<PhysicalASAPNodeId> = below
            .iter()
            .copied()
            .filter(|n| {
                !shareable.iter().any(|(other, other_below)| {
                    *other != id && below.contains(other) && other_below.contains(n)
                })
            })
            .collect();
        let mut once = dag.clone();
        once.nodes[position[&id]].materialization = Materialization::MatAtQuery(Medium::Memory);
        let state_bytes = per_computation(&once, entries, scan_lookback, horizons, data, models)?
            [&id]
            .held
            .map_or(0, |(bytes, _, _)| bytes);
        shared_nodes.push(SharedNodeCoefficients {
            cost_per_unit_rate: below.iter().map(|&n| per_rate(n)).sum(),
            work_ms: work(&below),
            state_bytes,
        });
        shared_node_keys.push(
            below
                .iter()
                .map(|n| shallow[position[n]])
                .collect::<Vec<u32>>(),
        );
    }
    let mut part_keys = vec![direct.iter().map(|id| keys[position[id]]).collect()];
    let mut slots = Vec::new();
    let mut slot_keys = Vec::new();
    let mut slot_reads = Vec::new();
    for node in &dag.nodes {
        if !is_private(node.id) || node.materialization == Materialization::NotMaterialized {
            continue;
        }
        let (part, reads) = walk(node.id);
        let via = Via::Slot(slots.len());
        lazy_of(&part, via);
        let at_ingestion = matches!(node.materialization, Materialization::MatAtIngestion(_));
        let rate_cost: f64 = part.iter().map(|&id| per_rate(id)).sum();
        let state: f64 = part.iter().map(|&id| kept_state(id)).sum();
        slots.push(SlotCoefficients {
            materialization: node.materialization,
            fixed_cost: match at_ingestion {
                true => rate_cost + state,
                false => state,
            },
            cost_per_unit_rate: match at_ingestion {
                true => 0.0,
                false => rate_cost,
            },
            work_ms: work(&part),
            // Equal builds over equal inputs are one node when shared (a
            // site's build at one threshold, or identical expressions).
            shared_site: Some((u64::from(keys[position[&node.id]]), 0)),
        });
        slot_keys.push(shallow[position[&node.id]]);
        part_keys.push(part.iter().map(|id| keys[position[id]]).collect());
        slot_reads.push(reads);
    }
    let child_via: Vec<Via> = child_roots
        .iter()
        .map(|child| {
            let reads_child = |reads: &[PhysicalASAPNodeId]| {
                reads.iter().any(|id| Some(keys[position[id]]) == *child)
            };
            match reads_child(&direct_reads) {
                true => Via::Direct,
                false => slot_reads
                    .iter()
                    .position(|reads| reads_child(reads))
                    .map_or(Via::Direct, Via::Slot),
            }
        })
        .collect();
    // The direct part at ingestion time: the whole sub-DAG placed at
    // ingestion time, its direct nodes counted.
    let mut at_ingestion = dag.clone();
    for node in &mut at_ingestion.nodes {
        node.output_state.timing = ExecutionTiming::IngestionTime;
    }
    let direct_at_ingestion = per_computation(
        &at_ingestion,
        entries,
        scan_lookback,
        horizons,
        data,
        models,
    )
    .ok()
    .map(|ingestion_priced| PartCoefficients {
        cost_per_unit_rate: direct
            .iter()
            .map(|id| {
                let p = &ingestion_priced[id];
                p.cpu_ops_per_second * prices.cost_per_cpu_op
                    + p.scanned_bytes_per_second * prices.cost_per_scanned_byte
                    + p.average_memory_bytes * prices.cost_per_memory_byte_second
                    + p.disk_bytes as f64 * prices.cost_per_disk_byte_second
            })
            .sum(),
        work_ms: 0.0,
    });
    // The group's own node built once at query time, in memory: the state
    // it holds instead of its working memory.
    let mut built_once = dag.nodes[position[&root]].clone();
    built_once.materialization = Materialization::MatAtQuery(Medium::Memory);
    let built_once_state_bytes = {
        let mut once_dag = dag.clone();
        once_dag.nodes[position[&root]] = built_once;
        let once_priced =
            per_computation(&once_dag, entries, scan_lookback, horizons, data, models)?;
        once_priced[&root].held.map_or(0, |(bytes, _, _)| bytes)
    };
    Ok(Some(Coefficients {
        coefficients: ChoiceCoefficients {
            direct: direct_coefficients,
            slots,
            child_via,
            shared_nodes,
        },
        lazy: LazyTerms {
            held,
            raw_scans,
            direct_at_ingestion,
            built_once_state_bytes,
            thresholds: Vec::new(),
        },
        scan_project_cost: match dag.nodes[position[&root]].payload {
            Operator::NonASAP(NonASAPOp::Scan { .. }) => {
                prices.cost_per_cpu_op * priced[&root].output.rows as f64
            }
            _ => 0.0,
        },
        root_key: shallow[position[&root]],
        shared_node_keys,
        root_producer_keys: producers_of
            .get(&root)
            .into_iter()
            .flatten()
            .filter(|p| is_private(**p))
            .map(|p| shallow[position[p]])
            .collect(),
        slot_keys,
        part_keys,
    }))
}

/// The plan-level charges of `lazy` for one choice in a plan (cost model
/// 1): each held term at its part's rate and its readers' latencies.
/// `part_rate(via)` is the rate of the part's consumers, `readers(via)` the
/// queries reaching it with their rates, `latency_ms(q)` query `q`'s
/// latency, `built_once` whether the group's own node is built once.
pub(crate) fn held_cost(
    lazy: &LazyTerms,
    built_once: bool,
    part_rate: impl Fn(Via) -> f64,
    readers: impl Fn(Via) -> Vec<(usize, f64)>,
    latency_ms: impl Fn(usize) -> f64,
    cost_per_memory_byte_second: f64,
) -> f64 {
    let mut cost = 0.0;
    let built_once_term = HeldTerm {
        via: Via::Direct,
        bytes: lazy.built_once_state_bytes,
        per_reader: false,
        own_node: true,
        node_key: u32::MAX,
    };
    let terms = lazy
        .held
        .iter()
        .filter(|term| !(built_once && term.own_node))
        .chain(built_once.then_some(&built_once_term));
    for term in terms {
        let (bytes, per_reader) = (term.bytes, term.per_reader);
        let readers = readers(term.via);
        let longest = readers
            .iter()
            .map(|&(q, _)| latency_ms(q))
            .fold(0.0, f64::max);
        let rate: f64 = readers.iter().map(|&(_, r)| r).sum();
        let evaluation_ms = match per_reader && rate > 0.0 {
            true => readers.iter().map(|&(q, r)| r * latency_ms(q)).sum::<f64>() / rate,
            false => longest,
        };
        cost += cost_per_memory_byte_second * bytes as f64 * part_rate(term.via) * evaluation_ms
            / 1_000.0;
    }
    cost
}

/// Each source's longest raw scan among `scans` (Stage 3 keeps it on disk
/// once).
pub(crate) fn raw_data_bytes<'s>(
    scans: impl IntoIterator<Item = &'s (String, u64)>,
) -> BTreeMap<&'s str, u64> {
    let mut longest: BTreeMap<&str, u64> = BTreeMap::new();
    for (source, bytes) in scans {
        let entry = longest.entry(source.as_str()).or_default();
        *entry = (*entry).max(*bytes);
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pass::subexpr::search::{
        at_thresholds, composed_roots, group_choices, identical_expression_variants, memo,
        resized_builds,
    };
    use crate::{computations, price_at_scale};
    use asap_logical_asap_optimization::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::{entry_with, lower_promql};
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::ir::schema::schema_support::with_promql_series_identity;
    use asap_types::workload::{
        DataArrival, Predictability, QueryRecurrence, RepeatedDemand, RepetitionInterval,
        SourceFacts,
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

    fn entries(intervals_ms: &[u32], targets: &[AccuracyTarget]) -> Vec<QueryWorkloadEntry> {
        intervals_ms
            .iter()
            .zip(targets)
            .map(|(&ms, target)| {
                let mut entry = entry_with(
                    target.clone(),
                    QueryRecurrence::Repeated(RepeatedDemand::FixedInterval(RepetitionInterval(
                        ms,
                    ))),
                );
                entry.predictability = Predictability::Predictable { known_at: None };
                entry
            })
            .collect()
    }

    /// For every plan of `queries`' memo (every choice and threshold, every
    /// Stage 2 materialization), Σ of its choices' coefficients times the
    /// true consumer rates, plus the lazy terms, is Stage 3's price.
    fn check(queries: &[&str], intervals_ms: &[u32]) -> Vec<Vec<Materialization>> {
        let exact = vec![AccuracyTarget::exact(); queries.len()];
        check_with(queries, intervals_ms, &exact)
    }

    /// [`check`] with each query's accuracy target; the Stage 2
    /// assignments of the plans checked.
    fn check_with(
        queries: &[&str],
        intervals_ms: &[u32],
        targets: &[AccuracyTarget],
    ) -> Vec<Vec<Materialization>> {
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .zip(targets)
            .map(|(q, target)| {
                let root = lower_promql(q, target.clone());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .enumerate()
            .collect();
        let per_computation = generate_per_computation_candidates(roots).unwrap();
        let entries = entries(intervals_ms, targets);
        let data = data();
        let models = PlanningModels::builtin();
        let settings = models.plan_selection;
        let CostModel::UsageOverTime(prices) = &settings.cost_model else {
            unreachable!()
        };
        let memo = memo(&per_computation, &entries);
        let mut cache = PriceCache::new(1.0);
        let parts: Vec<Vec<ChoiceParts>> = (0..memo.groups.len())
            .map(|g| {
                choice_parts(
                    &memo,
                    GroupId(g as u32),
                    &entries,
                    &data,
                    models,
                    &mut cache,
                )
                .unwrap()
            })
            .collect();
        let mut checked = Vec::new();
        let mut unplaced: Vec<Vec<Materialization>> = Vec::new();
        for choice in group_choices(&memo).unwrap() {
            // The plans the brute force prices: requirements met, scans
            // widened only through the identical-expression variants.
            let chosen: Vec<(GroupId, &ImplementationCandidate)> = choice
                .iter()
                .enumerate()
                .filter_map(|(g, c)| Some((GroupId(g as u32), &memo.candidates[g][(*c)?])))
                .collect();
            if !crate::pass::subexpr::windows::requirements_met(&chosen)
                || chosen
                    .iter()
                    .any(|(_, c)| c.origin == crate::pass::subexpr::CandidateOrigin::WidenedScans)
            {
                continue;
            }
            for plan in at_thresholds(&memo, &choice) {
                let Some(roots) = composed_roots(&memo, &per_computation, &plan) else {
                    continue;
                };
                let roots = crate::pass::subexpr::capability::share_equal_builds(
                    &roots,
                    &resized_builds(&memo, &choice, &plan),
                );
                // Widened variants (every metric's scans widened to one
                // label set) come after the others; they have no choices
                // yet (TODO: choices of widened candidates), and are only
                // priced exactly.
                let unwidened = 1 + asap_logical_asap_optimization::asap_aware_cse::identical_expression_pass2_candidates(
                    &roots,
                    &asap_logical_asap_optimization::asap_aware_cse::sharing_candidates(&roots),
                )
                .len();
                for (variant, roots) in identical_expression_variants(roots).into_iter().enumerate()
                {
                    let widened = variant >= unwidened;
                    let Ok(physicals) = optimize_physical_asap(
                        "L",
                        &roots,
                        &entries,
                        &data,
                        models.deployment_capabilities,
                    ) else {
                        continue;
                    };
                    for physical in physicals {
                        match priced_both(
                            &memo, &choice, &parts, &physical, &entries, &data, &models, prices,
                            &mut cache,
                        ) {
                            Some((expected, model, full)) => {
                                // The model's terms are a lower bound, and
                                // with the lazy terms the price itself.
                                assert!(
                                    model <= expected * (1.0 + 1e-9),
                                    "{queries:?} {:?}: model {model} over {expected}",
                                    physical.assignment
                                );
                                assert!(
                                    (full - expected).abs() <= 1e-9 * expected.abs(),
                                    "{queries:?} {:?}: {full} vs {expected}",
                                    physical.assignment
                                );
                                checked.push(physical.assignment.clone());
                            }
                            None if widened => {}
                            None => unplaced.push(physical.assignment.clone()),
                        }
                    }
                }
            }
        }
        assert!(
            unplaced.is_empty(),
            "{queries:?}: {} plans whose choices are not found",
            unplaced.len()
        );
        checked
    }

    /// Stage 3's price of `physical`, and the coefficients' (`None` when
    /// a group's choice is not found in it).
    #[allow(clippy::too_many_arguments)]
    fn priced_both(
        memo: &Memo,
        choice: &[Option<usize>],
        parts: &[Vec<ChoiceParts>],
        physical: &PhysicalCandidate,
        entries: &[QueryWorkloadEntry],
        data: &DataWorkload,
        models: &PlanningModels<'_>,
        prices: &crate::ResourceUnitCost,
        cache: &mut PriceCache,
    ) -> Option<(f64, f64, f64)> {
        let settings = models.plan_selection;
        let dag = &physical.dag;
        let recurrence: Vec<_> = entries.iter().map(|e| e.recurrence.clone()).collect();
        let ranges: Vec<_> = entries.iter().map(|e| e.range_evaluation).collect();
        let nodes = node_map(dag);
        let (cost, _) = price_at_scale(
            dag,
            &nodes,
            &kept_pane_chains(dag),
            &recurrence,
            &ranges,
            data,
            settings,
            1.0,
        )
        .ok()?;
        let expected = cost.cost_per_second?;

        let rates = computations(
            dag,
            &recurrence,
            &ranges,
            settings.non_repeating_query_horizon_seconds,
            None,
        )
        .ok()?;
        let reach = query_reach(dag, &nodes);
        let keys = shallow_keys(dag, cache.keys_mut());
        let position_of: HashMap<PhysicalASAPNodeId, usize> = dag
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id, i))
            .collect();
        let by_key = |key: u32| -> Vec<PhysicalASAPNodeId> {
            dag.nodes
                .iter()
                .zip(&keys)
                .filter(|(_, k)| **k == key)
                .map(|(n, _)| n.id)
                .collect()
        };
        let query_rate =
            |q: usize| crate::shared_evaluation_rate(&[&entries[q]], settings).unwrap();
        // Each node's producers, and the nodes below a node.
        let producers = |id: PhysicalASAPNodeId| -> Vec<PhysicalASAPNodeId> {
            dag.edges
                .iter()
                .filter(|e| e.consumer == id)
                .map(|e| e.producer)
                .collect()
        };
        let consumers =
            |id: PhysicalASAPNodeId| dag.edges.iter().filter(|e| e.producer == id).count();
        let below = |root: PhysicalASAPNodeId| -> Vec<PhysicalASAPNodeId> {
            let mut seen = HashSet::new();
            let mut stack = vec![root];
            let mut out = Vec::new();
            while let Some(id) = stack.pop() {
                if seen.insert(id) {
                    out.push(id);
                    stack.extend(producers(id));
                }
            }
            out
        };
        // The queries whose plan reaches each node at all.
        let starts = crate::query_starts(dag);
        let reaches_at_all = |id: PhysicalASAPNodeId| -> Vec<usize> {
            starts
                .iter()
                .enumerate()
                .filter(|(_, start)| start.iter().any(|s| below(*s).contains(&id)))
                .map(|(q, _)| q)
                .collect()
        };
        let kept = kept_pane_chains(dag);
        // A scan's horizon in the plan, or `None` when one reader has it
        // (apart, its reader's).
        let scan_variant = |s: PhysicalASAPNodeId| -> Option<u64> {
            (consumers(s) > 1).then(|| {
                windows_read(dag, &nodes, s, &kept)
                    .into_iter()
                    .max()
                    .unwrap_or(0)
            })
        };
        // Each needed group's choice in this plan: its candidate, with the
        // plan's materialization of its private builds.
        struct Placed<'p> {
            part: &'p ChoiceParts,
            root: PhysicalASAPNodeId,
            /// Whether `root` is its query-time copy.
            at_query_time: bool,
            /// Its copies at ingestion time (under ingestion-time builds).
            at_ingestion: Vec<PhysicalASAPNodeId>,
            slots: Vec<PhysicalASAPNodeId>,
        }
        let mut placed: Vec<Placed> = Vec::new();
        let mut used: HashSet<PhysicalASAPNodeId> = HashSet::new();
        // The plan's groups, parents first: the chosen ones, and the
        // children their choices read that the memo's candidates do not
        // list (their first candidate).
        let mut chosen: Vec<Option<usize>> = choice.to_vec();
        for g in (0..chosen.len()).rev() {
            let Some(c) = chosen[g] else { continue };
            for part in parts[g].iter().filter(|p| p.choice.candidate == c) {
                for child in part
                    .choice
                    .children
                    .iter()
                    .chain(part.choice.scan_children.iter().map(|(h, _)| h))
                {
                    chosen[child.0 as usize].get_or_insert(0);
                }
            }
        }
        for g in (0..chosen.len()).rev() {
            let Some(c) = chosen[g] else { continue };
            let sample = parts[g].iter().find(|p| p.choice.candidate == c)?;
            // The plan's copies of its node: equal shallow keys, reading
            // equal private producers.
            let group_queries = &memo.groups[g].queries;
            // The plan's copies of its node, by any of its candidate's
            // versions (thresholds): equal shallow keys, reading equal
            // private producers.
            let mut copies: Vec<PhysicalASAPNodeId> = Vec::new();
            for part in parts[g].iter().filter(|p| p.choice.candidate == c) {
                for id in by_key(part.root_key) {
                    let keys_below: Vec<u32> =
                        producers(id).iter().map(|p| keys[position_of[p]]).collect();
                    if !used.contains(&id)
                        && !copies.contains(&id)
                        && (part.choice.scan_horizon_ms.is_some()
                            || reaches_at_all(id).iter().any(|q| group_queries.contains(q)))
                        && part
                            .root_producer_keys
                            .iter()
                            .all(|k| keys_below.contains(k))
                    {
                        copies.push(id);
                    }
                }
            }
            if copies.is_empty() {
                // A scan every reader has apart (its readers').
                if sample.choice.scan_horizon_ms.is_some() {
                    continue;
                }
                return None;
            }
            let timing = |id: &PhysicalASAPNodeId| nodes[id].output_state.timing;
            let query_time: Vec<PhysicalASAPNodeId> = copies
                .iter()
                .copied()
                .filter(|id| timing(id) == ExecutionTiming::QueryTime)
                .collect();
            let at_ingestion: Vec<PhysicalASAPNodeId> = copies
                .iter()
                .copied()
                .filter(|id| timing(id) == ExecutionTiming::IngestionTime)
                .collect();
            let apart = query_time.len() > 1;
            let roots: Vec<(PhysicalASAPNodeId, bool)> = match query_time.is_empty() {
                true => vec![(at_ingestion[0], false)],
                false => query_time.iter().map(|&id| (id, true)).collect(),
            };
            for (k, &(root, at_query_time)) in roots.iter().enumerate() {
                let copy_of = match apart {
                    true => Some(
                        *reaches_at_all(root)
                            .iter()
                            .find(|q| group_queries.contains(q))?,
                    ),
                    false => None,
                };
                let mine = below(root);
                let scans_below: Vec<Option<u64>> = mine
                    .iter()
                    .filter(|id| {
                        matches!(nodes[id].payload, Operator::NonASAP(NonASAPOp::Scan { .. }))
                    })
                    .filter(|id| {
                        producers(root).contains(id)
                            || mine.iter().any(|m| producers(*m).contains(id))
                    })
                    .map(|&s| scan_variant(s))
                    .collect();
                let part = parts[g].iter().find(|p| {
                    p.choice.candidate == c
                        && p.choice.copy_of == copy_of
                        && p.build_keys
                            .iter()
                            .zip(&p.choice.assignment)
                            .all(|(key, m)| {
                                mine.iter().any(|id| {
                                    keys[position_of[id]] == *key && nodes[id].materialization == *m
                                })
                            })
                        && p.choice
                            .scan_children
                            .iter()
                            .all(|(_, v)| scans_below.contains(v))
                        && match p.choice.scan_horizon_ms {
                            None => true,
                            Some(h) => {
                                windows_read(dag, &nodes, root, &kept).into_iter().max() == Some(h)
                            }
                        }
                });
                let part = part?;
                // Apart scans below are this choice's.
                for (_, v) in &part.choice.scan_children {
                    if v.is_none() {
                        for id in &mine {
                            if matches!(
                                nodes[id].payload,
                                Operator::NonASAP(NonASAPOp::Scan { .. })
                            ) && consumers(*id) == 1
                            {
                                used.insert(*id);
                            }
                        }
                    }
                }
                let slots = part
                    .slot_keys
                    .iter()
                    .map(|key| {
                        mine.iter()
                            .copied()
                            .find(|id| keys[position_of[id]] == *key)
                    })
                    .collect::<Option<Vec<_>>>();
                let slots = slots?;
                placed.push(Placed {
                    part,
                    root,
                    at_query_time,
                    at_ingestion: match k {
                        0 => at_ingestion.clone(),
                        _ => Vec::new(),
                    },
                    slots,
                });
            }
        }
        let part_root = |p: &Placed, via: Via| match via {
            Via::Direct => p.root,
            Via::Slot(i) => p.slots[i],
        };
        // Latency: each query's sum of the work of the parts it reaches.
        let mut latency = vec![0.0; entries.len()];
        let mut counted_slots: HashSet<PhysicalASAPNodeId> = HashSet::new();
        for p in &placed {
            let vias = std::iter::once(Via::Direct).chain((0..p.slots.len()).map(Via::Slot));
            for via in vias {
                // A part's node several copies or groups share is one
                // node, its work counted once.
                if !counted_slots.insert(part_root(p, via)) {
                    continue;
                }
                let work = match via {
                    Via::Direct if !p.at_query_time => 0.0,
                    Via::Direct => p.part.choice.coefficients.direct.work_ms,
                    Via::Slot(i) => p.part.choice.coefficients.slots[i].work_ms,
                };
                for (q, reached) in reach.iter().enumerate() {
                    if reached.contains(&part_root(p, via)) {
                        latency[q] += work;
                    }
                }
            }
        }
        let mut actual = 0.0;
        let mut scans = Vec::new();
        // Shared nodes built once at query time: charged once, at their
        // rate, in place of each copy's per-consumer charge.
        let mut built_once_nodes: HashSet<PhysicalASAPNodeId> = HashSet::new();
        let mut held_extra = 0.0;
        let mut charged_slots: HashSet<PhysicalASAPNodeId> = HashSet::new();
        let mut charged_roots: HashSet<PhysicalASAPNodeId> = HashSet::new();
        for p in &placed {
            let coefficients = &p.part.choice.coefficients;
            let mut lazy = p.part.lazy.clone();
            // Two groups whose chosen nodes are one node (identical after a
            // shared build is resized): its consumers' rate is both groups',
            // charged once.
            let first_root = charged_roots.insert(p.root);
            if !first_root {
                lazy.held.retain(|term| term.via != Via::Direct);
                lazy.raw_scans.retain(|(_, _, via)| *via != Via::Direct);
            }
            if p.at_query_time && first_root {
                actual += coefficients.direct.cost_per_unit_rate * rates[&p.root].1;
                let mine = below(p.root);
                let mut matched: HashSet<PhysicalASAPNodeId> = HashSet::new();
                for (shared, sub) in coefficients
                    .shared_nodes
                    .iter()
                    .zip(&p.part.shared_node_keys)
                {
                    // Charged once per plan node at its rate: the node is
                    // one node for the copies or groups sharing it, built
                    // once when it (or a node above it) is materialized.
                    let Some(node) = mine.iter().copied().find(|id| {
                        keys[position_of[id]] == sub[0]
                            && !matched.contains(id)
                            && nodes[id].output_state.timing == ExecutionTiming::QueryTime
                    }) else {
                        continue;
                    };
                    matched.insert(node);
                    actual -= shared.cost_per_unit_rate * rates[&p.root].1;
                    let materialized =
                        nodes[&node].materialization != Materialization::NotMaterialized;
                    let part_held: Vec<HeldTerm> = lazy
                        .held
                        .iter()
                        .filter(|t| sub.contains(&t.node_key))
                        .filter(|t| !(materialized && t.node_key == sub[0]))
                        .copied()
                        .collect();
                    lazy.held.retain(|term| !sub.contains(&term.node_key));
                    if built_once_nodes.insert(node) {
                        actual += shared.cost_per_unit_rate * rates[&node].1;
                        let readers: Vec<(usize, f64)> = reach
                            .iter()
                            .enumerate()
                            .filter(|(_, r)| r.contains(&node))
                            .map(|(q, _)| (q, query_rate(q)))
                            .collect();
                        let rate: f64 = readers.iter().map(|(_, r)| r).sum();
                        let longest = readers.iter().map(|&(q, _)| latency[q]).fold(0.0, f64::max);
                        for term in part_held {
                            let evaluation_ms = match term.per_reader && rate > 0.0 {
                                true => {
                                    readers.iter().map(|&(q, r)| r * latency[q]).sum::<f64>() / rate
                                }
                                false => longest,
                            };
                            held_extra += prices.cost_per_memory_byte_second
                                * term.bytes as f64
                                * rates[&node].1
                                * evaluation_ms
                                / 1_000.0;
                        }
                        if materialized {
                            held_extra += prices.cost_per_memory_byte_second
                                * shared.state_bytes as f64
                                * rates[&node].1
                                * longest
                                / 1_000.0;
                        }
                    }
                }
            }
            for copy in &p.at_ingestion {
                let at_ingestion = p.part.lazy.direct_at_ingestion?;
                actual += at_ingestion.cost_per_unit_rate * rates[copy].1;
            }
            // A slot two copies share is one node, charged once, its held
            // memory too.
            for (i, slot) in coefficients.slots.iter().enumerate() {
                if charged_slots.insert(p.slots[i]) {
                    actual += slot.fixed_cost + slot.cost_per_unit_rate * rates[&p.slots[i]].1;
                } else {
                    lazy.held.retain(|term| term.via != Via::Slot(i));
                    lazy.raw_scans.retain(|(_, _, via)| *via != Via::Slot(i));
                }
            }
            let built_once = nodes[&p.root].materialization != Materialization::NotMaterialized
                && !is_build(&nodes[&p.root].payload)
                && first_root;
            // Only a query-time direct part holds memory or scans raw data.
            if !p.at_query_time {
                lazy.held.retain(|term| term.via != Via::Direct);
                lazy.raw_scans.retain(|(_, _, via)| *via != Via::Direct);
            }
            held_extra += held_cost(
                &lazy,
                built_once,
                |via| rates[&part_root(p, via)].1,
                |via| {
                    reach
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.contains(&part_root(p, via)))
                        .map(|(q, _)| (q, query_rate(q)))
                        .collect()
                },
                |q| latency[q],
                prices.cost_per_memory_byte_second,
            );
            for (source, bytes, via) in &lazy.raw_scans {
                if nodes[&part_root(p, *via)].output_state.timing == ExecutionTiming::QueryTime {
                    scans.push((source.clone(), *bytes));
                }
            }
        }
        let model = actual;
        let full = model
            + held_extra
            + raw_data_bytes(&scans)
                .values()
                .map(|&bytes| prices.cost_per_disk_byte_second * bytes as f64)
                .sum::<f64>();
        Some((expected, model, full))
    }

    #[test]
    fn coefficients_reproduce_stage_3_on_one_query_plans() {
        for query in [
            "sum by (job) (rate(m[5m]))",
            "max(avg_over_time(m[10m]))",
            "sum_over_time(m[5m])",
        ] {
            assert!(!check(&[query], &[60_000]).is_empty(), "{query}");
        }
    }

    /// A group both queries read: not materialized, built once at query
    /// time, and its builds at ingestion time, in every plan.
    #[test]
    fn coefficients_reproduce_stage_3_with_a_shared_group() {
        let checked = check(
            &[
                "sum by (job) (rate(m[5m]))",
                "sum by (job) (rate(m[5m])) * 2",
            ],
            &[60_000, 30_000],
        );
        let at = |m: fn(&Materialization) -> bool| checked.iter().flatten().any(m);
        assert!(at(|m| matches!(m, Materialization::MatAtQuery(_))));
        assert!(at(|m| matches!(m, Materialization::MatAtIngestion(_))));
    }

    /// A 5-minute sum read from its own panes or from a 1-hour sum's
    /// chain (the chain site's thresholds), built at query or ingestion
    /// time.
    #[test]
    fn coefficients_reproduce_stage_3_with_a_shared_pane_chain() {
        let checked = check(
            &["sum_over_time(m[5m])", "sum_over_time(m[1h])"],
            &[60_000, 60_000],
        );
        assert!(checked.len() > 2);
    }

    /// Three queries over one metric: a shared rate under two
    /// aggregations, and a sum over a longer window of the same scan.
    #[test]
    fn coefficients_reproduce_stage_3_on_a_mixed_workload() {
        let checked = check(
            &[
                "sum by (job) (rate(m[5m]))",
                "max(rate(m[5m]))",
                "sum_over_time(m[10m])",
            ],
            &[60_000, 30_000, 60_000],
        );
        assert!(!checked.is_empty());
    }

    /// A per-job sum and the total rolled up from it. (TODO: a sketch
    /// rollup, which reads its finer group's build rather than its node;
    /// the test does not place those plans yet.)
    #[test]
    fn coefficients_reproduce_stage_3_with_a_rollup() {
        assert!(!check(
            &["sum by (job) (rate(m[5m]))", "sum(rate(m[5m]))"],
            &[60_000, 60_000],
        )
        .is_empty());
    }

    /// The model restricted to each group choice ([`super::super::milp::lower_bound_check`])
    /// is a lower bound of the group choice's exact price.
    fn assert_model_is_a_lower_bound(
        queries: &[&str],
        intervals_ms: &[u32],
        targets: &[AccuracyTarget],
    ) {
        let roots: Vec<(usize, QueryRoot)> = queries
            .iter()
            .zip(targets)
            .map(|(q, target)| {
                let root = lower_promql(q, target.clone());
                QueryRoot::Operator(with_promql_series_identity(&root).unwrap())
            })
            .enumerate()
            .collect();
        let per_computation = generate_per_computation_candidates(roots).unwrap();
        let entries = entries(intervals_ms, targets);
        let data = data();
        let models = PlanningModels::builtin();
        let memo = memo(&per_computation, &entries);
        let input = super::super::milp::model_input_of(
            &memo,
            &entries,
            &data,
            models,
            std::time::Instant::now(),
        )
        .unwrap();
        let checked = super::super::milp::lower_bound_check(
            &memo,
            &per_computation,
            &input,
            &entries,
            &data,
            models,
        )
        .unwrap();
        assert!(!checked.is_empty());
        for (group_choice, exact, value) in checked {
            let value = value.unwrap_or_else(|| panic!("{queries:?} {group_choice:?}: infeasible"));
            assert!(
                value <= exact * (1.0 + 1e-9),
                "{queries:?} {group_choice:?}: model {value} over exact {exact}"
            );
        }
    }

    /// Three quantiles of one metric over a 1-minute, a 5-minute and an
    /// instant window (#840's shape), every 10 s; and a shared group with
    /// a rollup.
    #[test]
    fn the_model_is_a_lower_bound_of_every_group_choice() {
        let target = AccuracyTarget::max_error(0.01).failure_probability(0.001);
        assert_model_is_a_lower_bound(
            &[
                "quantile_over_time(0.5, m[1m])",
                "quantile_over_time(0.9, m[5m])",
                "quantile by (job) (0.5, m)",
            ],
            &[10_000, 10_000, 10_000],
            &[target.clone(), target.clone(), target],
        );
        let exact = AccuracyTarget::exact();
        assert_model_is_a_lower_bound(
            &[
                "sum by (job) (rate(m[5m]))",
                "sum(rate(m[5m]))",
                "sum_over_time(m[10m])",
            ],
            &[60_000, 30_000, 60_000],
            &[exact.clone(), exact.clone(), exact],
        );
    }

    /// #839's shape: sums over 10 minutes by job and in total (a rollup),
    /// and two 100-second quantiles over scans of one metric projecting
    /// different labels (a widened scan shares them).
    #[test]
    fn the_model_is_a_lower_bound_with_rollups_and_widened_scans() {
        let target = AccuracyTarget::max_error(0.05).failure_probability(0.3);
        assert_model_is_a_lower_bound(
            &[
                "sum by (job) (sum_over_time(m[10m]))",
                "sum(sum_over_time(m[10m]))",
                "avg by (job) (quantile_over_time(0.99, m[100s]))",
                "avg(quantile_over_time(0.95, m[100s]))",
            ],
            &[20_000, 20_000, 1_000, 1_000],
            &[target.clone(), target.clone(), target.clone(), target],
        );
    }

    /// Two quantiles within 1% and 2% sharing one KLL build at a size
    /// threshold.
    #[test]
    fn coefficients_reproduce_stage_3_with_a_shared_build() {
        let checked = check_with(
            &[
                "quantile_over_time(0.9, m[5m])",
                "quantile_over_time(0.9, m[5m])",
            ],
            &[60_000, 60_000],
            &[
                AccuracyTarget::max_error(0.01),
                AccuracyTarget::max_error(0.02),
            ],
        );
        assert!(!checked.is_empty());
    }
}
