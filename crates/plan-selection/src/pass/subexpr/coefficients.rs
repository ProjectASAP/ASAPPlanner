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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use asap_physical_asap_optimization::{optimize_physical_asap, PhysicalCandidate};
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::properties::ExecutionTiming;
use asap_types::ir::{ASAPOp, NonASAPOp, Operator, QueryRoot};
use asap_types::physical::{Materialization, Medium};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use super::milp::{Choice, ChoiceCoefficients, PartCoefficients, SlotCoefficients, Via};
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
    let queries = &memo.groups[group.0 as usize].queries;
    let group_entries: Vec<QueryWorkloadEntry> =
        queries.iter().map(|&q| entries[q].clone()).collect();
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
    for (index, candidate, thresholds) in &versions {
        let (index, candidate) = (*index, candidate);
        let roots = vec![QueryRoot::Operator(Rc::clone(&candidate.node)); queries.len().max(1)];
        let Ok(physicals) = optimize_physical_asap(
            "S",
            &roots,
            &group_entries,
            data,
            models.deployment_capabilities,
        ) else {
            continue;
        };
        let child_groups = children_of(memo, group, candidate);

        let mut children: HashSet<u32> = HashSet::new();
        let mut child_roots: Vec<Option<u32>> = Vec::new();
        for &child in &child_groups {
            let keys = child_keys(memo, child, entries, data, &models, cache);
            children.extend(keys.nodes.iter().copied());
            child_roots.push(keys.root);
        }
        let mut seen: Vec<Vec<Materialization>> = Vec::new();
        for physical in &physicals {
            let keys = keys_of(&physical.dag, cache.keys_mut());
            let shallow = shallow_keys(&physical.dag, cache.keys_mut());
            let private: Vec<bool> = keys.iter().map(|k| !children.contains(k)).collect();
            let builds: Vec<usize> = (0..physical.dag.nodes.len())
                .filter(|&i| private[i] && is_build(&physical.dag.nodes[i].payload))
                .collect();
            // Only the private builds are the choice's; any other
            // materialized node (the group's own node built once, a
            // child's build) is another variable of the model.
            let foreign = physical.dag.nodes.iter().enumerate().any(|(i, n)| {
                n.materialization != Materialization::NotMaterialized && !builds.contains(&i)
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
                memo,
                group,
                index,
                candidate,
                physical,
                &keys,
                &shallow,
                &private,
                &child_roots,
                &horizons_of(memo, thresholds),
                &group_entries,
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
                    children: child_groups.clone(),
                    assignment,
                    coefficients: parts.coefficients,
                },
                lazy: LazyTerms {
                    thresholds: thresholds.clone(),
                    ..parts.lazy
                },
                root_key: parts.root_key,
                root_producer_keys: parts.root_producer_keys,
                slot_keys: parts.slot_keys,
                build_keys: builds.iter().map(|&i| shallow[i]).collect(),
                part_keys: parts.part_keys,
            });
        }
    }
    Ok(out)
}

/// `candidate` rewritten at each combination of thresholds of its coupling
/// sites that it may read, as the brute-force search rewrites a plan's
/// candidates, with the index of each threshold. At a raw-horizon
/// threshold the candidate is unchanged and its query-time scans of the
/// site's source read the threshold's horizon ([`horizons_of`]). A
/// candidate without sites is itself.
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
                CouplingKind::RawHorizon => {
                    let own: Vec<scans::RawReader> = scans::raw_readers(memo, &version)
                        .into_iter()
                        .filter(|r| r.key == site.key)
                        .collect();
                    for (index, threshold) in
                        scans::horizon_thresholds(memo, site).iter().enumerate()
                    {
                        if own.iter().all(|r| scans::may_read_horizon(threshold, r)) {
                            next.extend(at(index, Some(version.clone())));
                        }
                    }
                }
            }
        }
        versions = next;
    }
    versions
}

/// The raw-data horizon of each source key `thresholds` fixes, in ms.
fn horizons_of(memo: &Memo, thresholds: &[(CouplingSite, usize)]) -> HashMap<u64, u64> {
    thresholds
        .iter()
        .filter(|(site, _)| site.kind == CouplingKind::RawHorizon)
        .filter_map(|(site, index)| {
            super::scans::horizon_thresholds(memo, site)
                .get(*index)
                .map(|t| (site.key, t.horizon_ms))
        })
        .collect()
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
    lazy: LazyTerms,
    root_key: u32,
    root_producer_keys: Vec<u32>,
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
    memo: &Memo,
    group: GroupId,
    _index: usize,
    candidate: &ImplementationCandidate,
    physical: &PhysicalCandidate,
    keys: &[u32],
    shallow: &[u32],
    private: &[bool],
    child_roots: &[Option<u32>],
    horizons: &HashMap<u64, u64>,
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
    let scan_lookback = scan_lookback_ms(memo, group);
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
    let _ = candidate;
    Ok(Some(Coefficients {
        coefficients: ChoiceCoefficients {
            direct: direct_coefficients,
            slots,
            child_via,
        },
        lazy: LazyTerms {
            held,
            raw_scans,
            direct_at_ingestion,
            built_once_state_bytes,
            thresholds: Vec::new(),
        },
        root_key: shallow[position[&root]],
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
        for choice in group_choices(&memo).unwrap() {
            for plan in at_thresholds(&memo, &choice) {
                let Some(roots) = composed_roots(&memo, &per_computation, &plan) else {
                    continue;
                };
                let roots = crate::pass::subexpr::capability::share_equal_builds(
                    &roots,
                    &resized_builds(&memo, &choice, &plan),
                );
                for roots in identical_expression_variants(roots) {
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
                        if let Some((expected, actual)) = priced_both(
                            &memo, &choice, &parts, &physical, &entries, &data, &models, prices,
                            &mut cache,
                        ) {
                            assert!(
                                (actual - expected).abs() <= 1e-9 * expected.abs(),
                                "{queries:?} {:?}: {actual} vs {expected}",
                                physical.assignment
                            );
                            checked.push(physical.assignment.clone());
                        }
                    }
                }
            }
        }
        checked
    }

    /// Stage 3's price of `physical`, and the coefficients' (`None` when
    /// a group's choice is not found in it).
    #[allow(clippy::too_many_arguments)]
    fn priced_both(
        _memo: &Memo,
        choice: &[Option<usize>],
        parts: &[Vec<ChoiceParts>],
        physical: &PhysicalCandidate,
        entries: &[QueryWorkloadEntry],
        data: &DataWorkload,
        models: &PlanningModels<'_>,
        prices: &crate::ResourceUnitCost,
        cache: &mut PriceCache,
    ) -> Option<(f64, f64)> {
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
        // The plan's groups: the chosen ones, and the children their
        // choices read that the memo's candidates do not list (their first
        // candidate).
        let mut chosen: Vec<Option<usize>> = choice.to_vec();
        let mut queue: Vec<usize> = (0..chosen.len()).filter(|&g| chosen[g].is_some()).collect();
        let mut order = Vec::new();
        while let Some(g) = queue.pop() {
            order.push(g);
            let c = chosen[g].expect("queued groups are chosen");
            for part in parts[g].iter().filter(|p| p.choice.candidate == c).take(1) {
                for child in &part.choice.children {
                    let h = child.0 as usize;
                    if chosen[h].is_none() {
                        chosen[h] = Some(0);
                        queue.push(h);
                    }
                }
            }
        }
        order.sort_unstable();
        for g in order {
            let c = chosen[g].expect("chosen");
            let candidates: Vec<&ChoiceParts> = parts[g]
                .iter()
                .filter(|p| p.choice.candidate == c)
                .collect();
            let part = candidates.into_iter().find(|p| {
                p.build_keys
                    .iter()
                    .zip(&p.choice.assignment)
                    .all(|(key, m)| {
                        by_key(*key)
                            .iter()
                            .any(|id| nodes[id].materialization == *m)
                    })
            });
            let part = part?;
            // The plan's copies of its node: equal shallow keys, reading
            // equal private producers.
            let copies: Vec<PhysicalASAPNodeId> = by_key(part.root_key)
                .into_iter()
                .filter(|id| !used.contains(id))
                .filter(|id| {
                    let producers: Vec<u32> = dag
                        .edges
                        .iter()
                        .filter(|e| e.consumer == *id)
                        .map(|e| keys[position_of[&e.producer]])
                        .collect();
                    part.root_producer_keys
                        .iter()
                        .all(|k| producers.contains(k))
                })
                .collect();
            used.extend(copies.iter().copied());
            let timing = |id: &PhysicalASAPNodeId| nodes[id].output_state.timing;
            let query_time = copies
                .iter()
                .copied()
                .find(|id| timing(id) == ExecutionTiming::QueryTime);
            let at_ingestion: Vec<PhysicalASAPNodeId> = copies
                .iter()
                .copied()
                .filter(|id| timing(id) == ExecutionTiming::IngestionTime)
                .collect();
            let root = query_time.or(at_ingestion.first().copied())?;
            let slots = part
                .slot_keys
                .iter()
                .map(|key| by_key(*key).first().copied())
                .collect::<Option<Vec<_>>>()?;
            placed.push(Placed {
                part,
                root,
                at_query_time: query_time.is_some(),
                at_ingestion,
                slots,
            });
        }
        let part_root = |p: &Placed, via: Via| match via {
            Via::Direct => p.root,
            Via::Slot(i) => p.slots[i],
        };
        // Latency: each query's sum of the work of the parts it reaches.
        let mut latency = vec![0.0; entries.len()];
        for p in &placed {
            let vias = std::iter::once(Via::Direct).chain((0..p.slots.len()).map(Via::Slot));
            for via in vias {
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
        for p in &placed {
            let coefficients = &p.part.choice.coefficients;
            if p.at_query_time {
                actual += coefficients.direct.cost_per_unit_rate * rates[&p.root].1;
            }
            for copy in &p.at_ingestion {
                let at_ingestion = p.part.lazy.direct_at_ingestion?;
                actual += at_ingestion.cost_per_unit_rate * rates[copy].1;
            }
            for (i, slot) in coefficients.slots.iter().enumerate() {
                actual += slot.fixed_cost + slot.cost_per_unit_rate * rates[&p.slots[i]].1;
            }
            let built_once = nodes[&p.root].materialization != Materialization::NotMaterialized
                && !is_build(&nodes[&p.root].payload);
            // Only a query-time direct part holds memory or scans raw data.
            let mut lazy = p.part.lazy.clone();
            if !p.at_query_time {
                lazy.held.retain(|term| term.via != Via::Direct);
                lazy.raw_scans.retain(|(_, _, via)| *via != Via::Direct);
            }
            actual += held_cost(
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
        actual += raw_data_bytes(&scans)
            .values()
            .map(|&bytes| prices.cost_per_disk_byte_second * bytes as f64)
            .sum::<f64>();
        Some((expected, actual))
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
    #[ignore = "a pane chain over a raw scan is no raw-horizon reader: the shared \
                scan's look-back (the longest window of its readers) is not a site \
                threshold, so a chain's input rows depend on other groups"]
    fn coefficients_reproduce_stage_3_with_a_shared_pane_chain() {
        let checked = check(
            &["sum_over_time(m[5m])", "sum_over_time(m[1h])"],
            &[60_000, 60_000],
        );
        assert!(checked.len() > 2);
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
