//! Display labels for the planning stages' candidates, for the
//! `stage_pipeline` viewer document only. Planning never reads them: they
//! are pure functions of the candidates' planning data (the Pass 1 choice,
//! the Pass 2 rules applied and the materialization assignment).

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_logical_asap_optimization::pass1::per_computation_candidates::{
    HowToComputeAggregateMeasure, PerComputationCandidates,
};
use asap_logical_asap_optimization::pass2::asap_aware_cse::PaneCandidate;
use asap_logical_asap_optimization::pass2::workload_candidates::Pass2Candidate;
use asap_physical_asap_optimization::PhysicalWorkloadPlan;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
use asap_types::ir::NonASAPOp;
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::Materialization;

use crate::planning_stages::PlanningStagesRun;

/// The label of every logical (`L<n>`) and physical (`P<n>`, `P<n>-m<k>`)
/// candidate of `run`, by id.
pub fn candidate_labels(run: &PlanningStagesRun) -> BTreeMap<String, String> {
    let logical = &run.stage1_logical_asap_optimization;
    let mut labels = BTreeMap::new();
    for candidate in logical {
        let mut label = if candidate.choice.is_empty() {
            candidate
                .roots
                .as_deref()
                .map(connected_candidate_label)
                .unwrap_or_else(|_| "Unimplemented candidate".into())
        } else {
            choice_label(&run.stage1_pass1_per_computation, &candidate.choice)
        };
        if let Some(pass2) = &candidate.pass2 {
            // The default search keeps only the selected candidate, so its
            // Pass 1 candidate may not be in the run.
            let pass1_roots = logical
                .get(pass2.from_pass1_position)
                .and_then(|c| c.roots.as_deref().ok())
                .unwrap_or_default();
            let candidate_roots = candidate.roots.as_deref().unwrap_or_default();
            label += &pass2_label(pass2, pass1_roots, candidate_roots);
        }
        labels.insert(candidate.id.clone(), label);
    }
    for candidate in &run.stage2_physical_asap_optimization {
        let logical = labels
            .get(&candidate.from_logical)
            .cloned()
            .unwrap_or_default();
        labels.insert(candidate.id.clone(), physical_label(&logical, candidate));
    }
    labels
}

/// Joint selections carry their actual connected DAG rather than a Cartesian
/// choice vector. Derive labels from that DAG so a sketch is never called exact.
fn connected_candidate_label(roots: &[QueryRoot]) -> String {
    let mut labels = Vec::new();
    let mut producers: BTreeMap<usize, (String, Vec<usize>)> = BTreeMap::new();
    for (query, root) in roots.iter().enumerate() {
        let mut states = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for operator in root_operators(root) {
            for node in OperatorNode::reachable(operator) {
                let identity = Rc::as_ptr(&node) as usize;
                if !seen.insert(identity) {
                    continue;
                }
                if matches!(
                    node.asap(),
                    Some(ASAPOp::SummaryAgg { .. } | ASAPOp::PaneChain { .. })
                ) {
                    let name = build_label(&node.operator);
                    if !states.contains(&name) {
                        states.push(name.clone());
                    }
                    producers
                        .entry(identity)
                        .or_insert_with(|| (name, Vec::new()))
                        .1
                        .push(query);
                } else if matches!(node.non_asap(), Some(NonASAPOp::Scan { .. })) {
                    producers
                        .entry(identity)
                        .or_insert_with(|| ("Scan".into(), Vec::new()))
                        .1
                        .push(query);
                }
            }
        }
        labels.push(format!(
            "Q{} {}",
            query + 1,
            if states.is_empty() {
                "exact".into()
            } else {
                states.join(" + ")
            }
        ));
    }
    let mut shared: Vec<_> = producers
        .values()
        .filter(|(_, readers)| readers.len() > 1)
        .map(|(name, readers)| format!("shared {name} ({})", query_names(readers)))
        .collect();
    shared.sort();
    labels.extend(shared);
    labels.join(" · ")
}

/// The query (index into `inventory.roots`) each target of `inventory` is
/// listed under: the first that reads it.
pub fn query_of_each_target<Id>(inventory: &PerComputationCandidates<Id>) -> Vec<usize> {
    inventory
        .targets
        .iter()
        .map(|target| {
            inventory
                .roots
                .iter()
                .position(|(_, root)| {
                    root_operators(root).into_iter().any(|node| {
                        OperatorNode::reachable(node)
                            .iter()
                            .any(|n| Rc::ptr_eq(n, &target.aggregate))
                    })
                })
                .expect("every target is reached from a root")
        })
        .collect()
}

/// A Pass 1 per-computation candidate, e.g. "exact", "Sum acc" or "Kll".
pub fn computation_label(candidate: &HowToComputeAggregateMeasure) -> String {
    match candidate {
        HowToComputeAggregateMeasure::Exact => "exact".to_string(),
        HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
            let kinds: Vec<String> = kinds.iter().map(|kind| format!("{kind:?}")).collect();
            format!("{} acc", kinds.join("/"))
        }
        HowToComputeAggregateMeasure::Summary(algorithm) => sketch_name(algorithm),
    }
}

/// A Pass 1 choice, e.g. "Q1 exact · Q2 CMS+heap" (exact accumulators in
/// parentheses), each target listed under the first query that reads it.
pub fn choice_label<Id>(inventory: &PerComputationCandidates<Id>, choice: &[usize]) -> String {
    let query_of_target = query_of_each_target(inventory);
    // Each target's chosen candidate, with the targets it absorbs when the
    // candidate is its fused computation's.
    let chosen: Vec<_> = inventory
        .targets
        .iter()
        .zip(choice)
        .map(|(target, &index)| target.computation_for(index).expect("a listed candidate"))
        .collect();
    // Targets a chosen fused computation absorbs are not computed.
    let absorbed: Vec<usize> = chosen
        .iter()
        .filter_map(|&(_, absorbs)| absorbs)
        .flatten()
        .copied()
        .collect();
    (0..inventory.roots.len())
        .map(|query| {
            let mut sketches = Vec::new();
            let mut accumulators = Vec::new();
            for (t, (&(candidate, absorbs), &target_query)) in
                chosen.iter().zip(&query_of_target).enumerate()
            {
                if target_query != query || absorbed.contains(&t) {
                    continue;
                }
                match candidate {
                    HowToComputeAggregateMeasure::Exact => {}
                    HowToComputeAggregateMeasure::ExactAccumulator(kinds) => {
                        // e.g. "Sum acc", "Sum/Count acc" for an avg, or
                        // "nested Sum acc" for a fused nested sum.
                        let kinds: Vec<String> =
                            kinds.iter().map(|kind| format!("{kind:?}")).collect();
                        let nested = if absorbs.is_some() { "nested " } else { "" };
                        accumulators.push(format!("{nested}{} acc", kinds.join("/")))
                    }
                    HowToComputeAggregateMeasure::Summary(algorithm) => {
                        sketches.push(sketch_name(algorithm))
                    }
                }
            }
            let mut text = format!("Q{} ", query + 1);
            text += &if sketches.is_empty() {
                "exact".to_string()
            } else {
                sketches.join(" + ")
            };
            if !accumulators.is_empty() {
                text += &format!(" ({})", accumulators.join(", "));
            }
            text
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// What a Pass 2 candidate adds to its Pass 1 candidate's label, e.g.
/// " · one UnivMon for Q1, Q3 · Kll for Q1 in 5 × 1m panes · Sum acc for Q2
/// rolled up from Q1 · shared Scan (Q1, Q2)". `pass1_roots` are the roots
/// of the Pass 1 candidate it derives from, which hold the shared summary
/// builds and the rolled-up builds; `candidate_roots` are its own, which hold its
/// panes, rankings and shared nodes. Readers are found by structure, since
/// later rules rebuild the nodes a rule saw.
pub fn pass2_label(
    pass2: &Pass2Candidate,
    pass1_roots: &[QueryRoot],
    candidate_roots: &[QueryRoot],
) -> String {
    let mut label = String::new();
    // Summary-capability rule, e.g. "UnivMon for Q1, Q3".
    let summaries: Vec<String> = pass2
        .shared_builds
        .iter()
        .map(|builds| {
            let shared = |node: &OperatorNode| builds.iter().any(|b| same_computation(node, b));
            let queries = readers_in(&[pass1_roots, candidate_roots], shared);
            let family = match builds[0].asap().and_then(ASAPOp::produced_state) {
                Some(FieldDataType::Sketch(kind, _)) => format!("{:?}", kind.algorithm()),
                _ => builds[0].operator.kind_name().to_string(),
            };
            for_queries(family, &queries)
        })
        .collect();
    if !summaries.is_empty() {
        label += &format!(" · one {}", summaries.join(", one "));
    }
    // Window-composition rule, e.g. "Kll for Q1 in 5 × 1m panes": the
    // queries that read the pane chain the build became. Identical builds
    // in different queries become one structurally identical chain, listed
    // once with all its readers. A chain several builds share is listed
    // once, with the queries that merge only its newest panes, e.g. "one
    // Kll for Q1, Q2 in 30 × 10s panes (Q1 newest 6)".
    let candidates = &pass2.pane_candidates;
    let shared_member = |i: usize| {
        pass2
            .shared_pane_chains
            .iter()
            .any(|shared| shared.members.contains(&i))
    };
    let own: Vec<String> = candidates
        .iter()
        .enumerate()
        .filter(|&(i, candidate)| {
            !shared_member(i)
                && !candidates[..i]
                    .iter()
                    .any(|earlier| same_pane_chain(earlier, candidate))
        })
        .map(|(_, candidate)| {
            let chain = |node: &OperatorNode| {
                is_pane_chain_of(
                    node,
                    &candidate.build,
                    candidate.pane_width_ms,
                    candidate.pane_count,
                    false,
                )
            };
            format!(
                "{} in {} × {} panes",
                for_queries(
                    state_name(&candidate.build),
                    &readers(candidate_roots, chain)
                ),
                candidate.pane_count,
                duration_name(candidate.pane_width_ms)
            )
        })
        .collect();
    let shared = pass2.shared_pane_chains.iter().map(|shared| {
        let build = &candidates[shared.members[0]].build;
        // The chain is one member's build in panes (a coarser member reads
        // it rolled up).
        let chain = |node: &OperatorNode| {
            shared.members.iter().any(|&m| {
                is_pane_chain_of(
                    node,
                    &candidates[m].build,
                    shared.pane_width_ms,
                    shared.pane_count,
                    true,
                )
            })
        };
        let mut text = format!(
            "one {} in {} × {} panes",
            for_queries(state_name(build), &readers(candidate_roots, chain)),
            shared.pane_count,
            duration_name(shared.pane_width_ms)
        );
        let mut counts: Vec<u32> = shared
            .members
            .iter()
            .map(|&m| {
                u32::try_from(candidates[m].window_ms() / shared.pane_width_ms).unwrap_or(u32::MAX)
            })
            .filter(|&n| n < shared.pane_count)
            .collect();
        counts.sort_unstable();
        counts.dedup();
        let newest: Vec<String> = counts
            .into_iter()
            .map(|n| {
                let reads_newest = |node: &OperatorNode| match node.asap() {
                    Some(ASAPOp::SummaryMerge {
                        children,
                        newest_panes,
                        ..
                    }) => *newest_panes == Some(n) && children.iter().any(|c| chain(c)),
                    _ => false,
                };
                format!(
                    "{} newest {n}",
                    query_names(&readers(candidate_roots, reads_newest))
                )
            })
            .collect();
        if !newest.is_empty() {
            text += &format!(" ({})", newest.join(", "));
        }
        text
    });
    let panes: Vec<String> = own.into_iter().chain(shared).collect();
    if !panes.is_empty() {
        label += &format!(" · {}", panes.join(", "));
    }
    // Rollup rule, e.g. "Sum acc for Q1 rolled up from Q2", then exact
    // top-k reuse, e.g. "top-k for Q1, Q2 from one top-10": the queries
    // that read the ranking.
    let reused: Vec<String> = pass2
        .rollup_candidates
        .iter()
        .map(|candidate| {
            let roots = [pass1_roots, candidate_roots];
            let finer = readers_in(&roots, |n| same_computation(n, &candidate.finer));
            let coarse = readers_in(&roots, |n| same_computation(n, &candidate.coarse));
            match finer.is_empty() {
                true => format!(
                    "{} rolled up",
                    for_queries(state_name(&candidate.coarse), &coarse)
                ),
                false => format!(
                    "{} rolled up from {}",
                    for_queries(state_name(&candidate.coarse), &coarse),
                    query_names(&finer)
                ),
            }
        })
        .chain(pass2.topk_reuse_candidates.iter().map(|candidate| {
            let k = match candidate.ranking.non_asap() {
                Some(NonASAPOp::Limit { n: Some(k), .. }) => *k,
                _ => 0,
            };
            let queries = readers(candidate_roots, |n| same_computation(n, &candidate.ranking));
            format!("{} from one top-{k}", for_queries("top-k".into(), &queries))
        }))
        .collect();
    if !reused.is_empty() {
        label += &format!(" · {}", reused.join(", "));
    }
    // Identical-expression rule, e.g. "Scan (Q1, Q2)": each shared node's
    // operator kind and the queries that read it.
    let shared: Vec<String> = pass2
        .sharing_candidates
        .iter()
        .map(|candidate| {
            let node = &candidate.nodes[0];
            let queries = readers(candidate_roots, |n| same_computation(n, node));
            match queries.is_empty() {
                true => node.operator.kind_name().to_string(),
                false => format!("{} ({})", node.operator.kind_name(), query_names(&queries)),
            }
        })
        .collect();
    if !shared.is_empty() {
        label += &format!(" · shared {}", shared.join(", "));
    }
    label
}

/// `what` with the queries that read it, e.g. "Kll for Q1, Q3"; `what`
/// alone when none is found (a node later rules rebuilt).
fn for_queries(what: String, queries: &[usize]) -> String {
    match queries.is_empty() {
        true => what,
        false => format!("{what} for {}", query_names(queries)),
    }
}

/// The readers of a node `matches` in the first of `roots` where any query
/// reads one.
fn readers_in(roots: &[&[QueryRoot]], matches: impl Fn(&OperatorNode) -> bool) -> Vec<usize> {
    roots
        .iter()
        .map(|roots| readers(roots, &matches))
        .find(|queries| !queries.is_empty())
        .unwrap_or_default()
}

/// Query names for query indices, e.g. "Q1, Q3".
fn query_names(queries: &[usize]) -> String {
    queries
        .iter()
        .map(|q| format!("Q{}", q + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The queries (indices into `roots`) from which a node `matches` is
/// reachable.
fn readers(roots: &[QueryRoot], matches: impl Fn(&OperatorNode) -> bool) -> Vec<usize> {
    roots
        .iter()
        .enumerate()
        .filter(|(_, root)| {
            root_operators(root)
                .into_iter()
                .flat_map(OperatorNode::reachable)
                .any(|n| matches(&n))
        })
        .map(|(q, _)| q)
        .collect()
}

/// Whether `a` is `b` or computes the same thing: the same operators,
/// schemas and children, whatever guarantees, timings and materializations
/// later steps attached.
fn same_computation(a: &OperatorNode, b: &OperatorNode) -> bool {
    std::ptr::eq(a, b)
        || (a.operator.map_children(|_| ()) == b.operator.map_children(|_| ())
            && a.result_kind == b.result_kind
            && a.schema == b.schema
            && a.children().len() == b.children().len()
            && a.children()
                .iter()
                .zip(b.children())
                .all(|(x, y)| same_computation(x, y)))
}

/// Whether `node` is a pane chain `build` became: the same family, update,
/// grouping and filter, over the build's window input, in `chain_pane_count`
/// panes of `chain_pane_width_ms`. With `any_size`, the family at any size:
/// a shared chain is sized for all its readers.
fn is_pane_chain_of(
    node: &OperatorNode,
    build: &OperatorNode,
    chain_pane_width_ms: u64,
    chain_pane_count: u32,
    any_size: bool,
) -> bool {
    let (
        Some(ASAPOp::PaneChain {
            child,
            family,
            input,
            reduction,
            grouping,
            filter,
            pane_width_ms,
            pane_count,
        }),
        Some(ASAPOp::SummaryAgg {
            child: range,
            family: build_family,
            input: build_input,
            reduction: build_reduction,
            grouping: build_grouping,
            filter: build_filter,
        }),
    ) = (node.asap(), build.asap())
    else {
        return false;
    };
    (*pane_width_ms, *pane_count) == (chain_pane_width_ms, chain_pane_count)
        && (family == build_family
            || any_size
                && match (family, build_family) {
                    (FieldDataType::Sketch(a, g), FieldDataType::Sketch(b, h)) => {
                        a.algorithm() == b.algorithm() && g == h
                    }
                    _ => false,
                })
        && input == build_input
        && reduction == build_reduction
        && grouping == build_grouping
        && filter == build_filter
        && range
            .children()
            .first()
            .is_some_and(|window_input| same_computation(child, window_input))
}

/// Whether two pane candidates become the same pane chain: the same
/// computation in the same panes.
fn same_pane_chain(a: &PaneCandidate, b: &PaneCandidate) -> bool {
    (a.pane_width_ms, a.pane_count) == (b.pane_width_ms, b.pane_count)
        && same_computation(&a.build, &b.build)
}

/// The state a summary build keeps, e.g. "Kll" or "Rate acc".
fn state_name(build: &OperatorNode) -> String {
    match build.asap().and_then(ASAPOp::produced_state) {
        Some(FieldDataType::Sketch(kind, _)) => sketch_name(kind.algorithm()),
        Some(FieldDataType::ExactAggregate(kind, _)) => format!("{kind:?} acc"),
        _ => build.operator.kind_name().to_string(),
    }
}

/// `ms` in the largest whole unit: "1m", "10s", "250ms".
fn duration_name(ms: u64) -> String {
    match ms {
        ms if ms % 3_600_000 == 0 => format!("{}h", ms / 3_600_000),
        ms if ms % 60_000 == 0 => format!("{}m", ms / 60_000),
        ms if ms % 1_000 == 0 => format!("{}s", ms / 1_000),
        ms => format!("{ms}ms"),
    }
}

/// `logical` followed by what the candidate materializes, e.g.
/// "… · ingestion time: Kll for Q1, 5 × 1m panes · query time, shared:
/// DDSketch";
/// just `logical` when nothing is.
pub fn physical_label(logical: &str, candidate: &PhysicalWorkloadPlan) -> String {
    let mut parts: Vec<(&str, Vec<String>)> = Vec::new();
    for (id, materialization) in &candidate.materialization {
        let phase = match materialization {
            Materialization::NotMaterialized => continue,
            Materialization::MatAtIngestion(_) => "ingestion time",
            Materialization::MatAtQuery(_) => "query time, shared",
            Materialization::MatAtQueryKeptAcrossEvaluations(_) => "query time, kept",
        };
        let node = candidate
            .dag
            .nodes
            .iter()
            .find(|n| n.id == *id)
            .expect("an assigned node is in the DAG");
        let label = match &node.payload {
            // Name the chain: chains of one family and pane count differ by
            // their readers and pane width.
            Operator::ASAP(ASAPOp::PaneChain {
                pane_width_ms,
                pane_count,
                ..
            }) => format!(
                "{}, {pane_count} × {} panes",
                for_queries(
                    state_label(&node.payload),
                    &dag_readers(&candidate.dag, *id)
                ),
                duration_name(*pane_width_ms)
            ),
            payload => build_label(payload),
        };
        match parts.iter_mut().find(|(p, _)| *p == phase) {
            Some((_, labels)) => labels.push(label),
            None => parts.push((phase, vec![label])),
        }
    }
    std::iter::once(logical.to_string())
        .chain(
            parts
                .into_iter()
                .map(|(phase, labels)| format!("{phase}: {}", labels.join(", "))),
        )
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A summary build, e.g. "Kll ×5 panes" or "Sum acc".
fn build_label<C>(payload: &Operator<C>) -> String {
    match payload {
        Operator::ASAP(ASAPOp::PaneChain { pane_count, .. }) => {
            format!("{} ×{pane_count} panes", state_label(payload))
        }
        _ => state_label(payload),
    }
}

/// The state a summary build or pane chain keeps, e.g. "Kll" or "Sum acc";
/// any other operator's kind.
fn state_label<C>(payload: &Operator<C>) -> String {
    match payload {
        Operator::ASAP(ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. }) => {
            match family {
                FieldDataType::Sketch(kind, _) => sketch_name(kind.algorithm()),
                FieldDataType::ExactAggregate(kind, _) => format!("{kind:?} acc"),
                _ => payload.kind_name().to_string(),
            }
        }
        _ => payload.kind_name().to_string(),
    }
}

/// The queries (in workload order) whose root reaches node `id` of `dag`.
fn dag_readers(dag: &PhysicalASAPDAG, id: PhysicalASAPNodeId) -> Vec<usize> {
    // Query `q` is the scalar root keyed `q` when there is one, and
    // otherwise the next operator root.
    let mut operator_roots = dag.roots.iter();
    let query_starts: Vec<Vec<PhysicalASAPNodeId>> = (0..dag.roots.len() + dag.scalar_roots.len())
        .map(|q| match dag.scalar_roots.iter().find(|(i, _)| *i == q) {
            Some((_, expr)) => expr.operator_refs().into_iter().copied().collect(),
            None => operator_roots.next().into_iter().copied().collect(),
        })
        .collect();
    query_starts
        .into_iter()
        .enumerate()
        .filter(|(_, starts)| {
            let mut seen = std::collections::HashSet::new();
            let mut stack = starts.clone();
            while let Some(node) = stack.pop() {
                if node == id {
                    return true;
                }
                if seen.insert(node) {
                    stack.extend(
                        dag.edges
                            .iter()
                            .filter(|e| e.consumer == node)
                            .map(|e| e.producer),
                    );
                }
            }
            false
        })
        .map(|(q, _)| q)
        .collect()
}

/// A sketch algorithm's display name, e.g. "CMS+heap" or "Kll".
fn sketch_name(algorithm: &SketchAlgorithm) -> String {
    match algorithm {
        SketchAlgorithm::CmsWithHeap => "CMS+heap".to_string(),
        SketchAlgorithm::CountSketchWithHeap => "CountSketch+heap".to_string(),
        other => format!("{other:?}"),
    }
}

fn root_operators(root: &QueryRoot) -> Vec<&Rc<OperatorNode>> {
    match root {
        QueryRoot::Operator(node) => vec![node],
        QueryRoot::Scalar(expr) => expr.operator_refs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner_layering_examples::example1;
    use crate::planning_stages::lower_promql_roots;
    use asap_logical_asap_optimization::pass1::per_computation_candidates::{
        compose_logical_candidate, generate_per_computation_candidates,
    };

    /// Selected DAG labels expose accumulator state without a legacy choice ID.
    #[test]
    fn connected_labels_describe_selected_states() {
        let workload = example1();
        let inventory = generate_per_computation_candidates(
            lower_promql_roots(&workload)
                .unwrap()
                .into_iter()
                .enumerate()
                .collect(),
        )
        .unwrap();
        let choice = vec![0; inventory.targets.len()];
        let roots: Vec<_> = compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let label = connected_candidate_label(&roots);
        assert!(label.contains("Q1 ") && label.contains("Q2 "));
        assert!(label.contains("acc"), "{label}");
    }

    /// A state reused by two query outputs is named once in the sharing suffix.
    #[test]
    fn connected_labels_name_actual_shared_state() {
        let workload = example1();
        let inventory = generate_per_computation_candidates(
            lower_promql_roots(&workload)
                .unwrap()
                .into_iter()
                .enumerate()
                .collect(),
        )
        .unwrap();
        let roots =
            compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()]).unwrap();
        let label = connected_candidate_label(&[roots[1].1.clone(), roots[1].1.clone()]);
        assert!(label.contains("shared Sum acc (Q1, Q2)"), "{label}");
        assert_eq!(label.matches("shared Sum acc").count(), 1);
    }

    /// Two physical candidates that each maintain a different query's pane
    /// chain at ingestion time get different labels, even when both chains
    /// have the same family and pane count (6 × 10s for Q1, 6 × 30s for Q2).
    #[test]
    fn physical_labels_name_the_pane_chain_maintained_at_ingestion() {
        use asap_logical_asap_optimization::pass2::asap_aware_cse::{
            pane_candidates, window_composition_pass2_candidates,
        };
        use asap_physical_asap_optimization::optimize_physical_asap;
        use asap_types::workload::PlanningWorkload;

        let workload: PlanningWorkload = serde_json::from_str(
            r#"{"query_workload": {"language": "promql", "query_batch": null, "repeating_queries": [
              {"query": "quantile_over_time(0.5, data[1m])", "demand": {"fixed_interval": 10000},
               "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
               "predictability": {"predictable": {"known_at": null}}},
              {"query": "quantile_over_time(0.5, data[3m])", "demand": {"fixed_interval": 30000},
               "requirements": {"accuracy": {"max_error": 0.01, "failure_probability": 0.001}},
               "predictability": {"predictable": {"known_at": null}}}]},
             "data_workload": {"arrival": {"ingesting": {"rows_per_second": 666.67}},
              "sources": {"data": {"series_count": 10000, "metric_type": "gauge", "sample_interval": 15000}}}}"#,
        )
        .unwrap();
        let inventory = generate_per_computation_candidates(
            lower_promql_roots(&workload)
                .unwrap()
                .into_iter()
                .enumerate()
                .collect(),
        )
        .unwrap();
        let choice: Vec<usize> = inventory
            .targets
            .iter()
            .map(|target| {
                target
                    .ways_to_compute
                    .iter()
                    .position(|c| {
                        *c == HowToComputeAggregateMeasure::Summary(SketchAlgorithm::DDSketch)
                    })
                    .expect("a DDSketch candidate")
            })
            .collect();
        let summary: Vec<_> = compose_logical_candidate(&inventory, &choice)
            .unwrap()
            .into_iter()
            .map(|(_, root)| root)
            .collect();
        let entries: Vec<_> = workload.query_workload.entries().collect();
        let grids: Vec<_> = entries.iter().map(|e| e.evaluation_grid_ms()).collect();
        let panes = pane_candidates(&summary, &grids);
        let roots = window_composition_pass2_candidates(&summary, &panes)
            .unwrap()
            .into_iter()
            .find(|c| c.pane_candidates.len() == 2 && c.shared_chains.is_empty())
            .expect("each query's DDSketch in its own pane chain")
            .roots
            .unwrap();
        let plans = optimize_physical_asap(
            "L1",
            &roots,
            &entries,
            &workload.data_workload.clone().unwrap_or_default(),
            false,
        )
        .unwrap();
        // The plans that maintain exactly one pane chain at ingestion time.
        let one_chain_at_ingestion: Vec<String> = plans
            .iter()
            .filter(|plan| {
                plan.materialization
                    .iter()
                    .filter(|(id, m)| {
                        matches!(m, Materialization::MatAtIngestion(_))
                            && matches!(
                                plan.dag
                                    .nodes
                                    .iter()
                                    .find(|n| n.id == **id)
                                    .unwrap()
                                    .payload,
                                Operator::ASAP(ASAPOp::PaneChain { .. })
                            )
                    })
                    .count()
                    == 1
            })
            .map(|plan| physical_label("L1", plan))
            .collect();
        let ingestion = |label: &String| {
            label
                .split(" · ")
                .find(|p| p.starts_with("ingestion time"))
                .map(str::to_string)
        };
        assert!(
            one_chain_at_ingestion
                .iter()
                .any(|l| ingestion(l).as_deref()
                    == Some("ingestion time: DDSketch for Q1, 6 × 10s panes")),
            "{one_chain_at_ingestion:#?}"
        );
        assert!(
            one_chain_at_ingestion
                .iter()
                .any(|l| ingestion(l).as_deref()
                    == Some("ingestion time: DDSketch for Q2, 6 × 30s panes")),
            "{one_chain_at_ingestion:#?}"
        );
        let mut distinct = one_chain_at_ingestion.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(
            distinct.len(),
            one_chain_at_ingestion.len(),
            "{one_chain_at_ingestion:#?}"
        );
    }

    /// Raw logical operators have an exact label, rather than an empty choice label.
    #[test]
    fn connected_labels_name_exact_results() {
        let roots = lower_promql_roots(&example1()).unwrap();
        assert_eq!(connected_candidate_label(&roots), "Q1 exact · Q2 exact");
    }
}
