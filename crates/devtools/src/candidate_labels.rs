//! Display labels for the planning stages' candidates, for the
//! `stage_pipeline` viewer document only. Planning never reads them: they
//! are pure functions of the candidates' planning data (the logical roots
//! and the materialization assignment).

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_logical_asap_optimization::pass1::HowToComputeAggregateMeasure;
use asap_types::ir::physical_export::{PhysicalASAPDAG, PhysicalASAPNodeId};
use asap_types::ir::schema::{FieldDataType, SketchAlgorithm};
use asap_types::ir::NonASAPOp;
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::physical::PhysicalWorkloadPlan;

use crate::planning_stages::PlanningStagesRun;

/// The label of every logical and physical candidate the document shows
/// of `run` ([`PlanningStagesRun::logical_candidates`],
/// [`PlanningStagesRun::physical_candidates`]), by id.
pub fn candidate_labels(run: &PlanningStagesRun) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    for candidate in run.logical_candidates() {
        let label = candidate
            .roots
            .as_deref()
            .map(connected_candidate_label)
            .unwrap_or_else(|_| "Unimplemented candidate".into());
        labels.insert(candidate.id.clone(), label);
    }
    for candidate in &run.physical_candidates() {
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

/// `what` with the queries that read it, e.g. "Kll for Q1, Q3"; `what`
/// alone when none is found (a node later rules rebuilt).
fn for_queries(what: String, queries: &[usize]) -> String {
    match queries.is_empty() {
        true => what,
        false => format!("{what} for {}", query_names(queries)),
    }
}

/// Query names for query indices, e.g. "Q1, Q3".
fn query_names(queries: &[usize]) -> String {
    queries
        .iter()
        .map(|q| format!("Q{}", q + 1))
        .collect::<Vec<_>>()
        .join(", ")
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
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &lower_promql_roots(&workload).unwrap(),
            ),
        )
        .unwrap();
        let choice = vec![0; inventory.targets.len()];
        let roots: Vec<_> = compose_logical_candidate(&inventory, &choice).unwrap();
        let label = connected_candidate_label(&roots);
        assert!(label.contains("Q1 ") && label.contains("Q2 "));
        assert!(label.contains("acc"), "{label}");
    }

    /// A state reused by two query outputs is named once in the sharing suffix.
    #[test]
    fn connected_labels_name_actual_shared_state() {
        let workload = example1();
        let inventory = generate_per_computation_candidates(
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &lower_promql_roots(&workload).unwrap(),
            ),
        )
        .unwrap();
        let roots =
            compose_logical_candidate(&inventory, &vec![0; inventory.targets.len()]).unwrap();
        let label = connected_candidate_label(&[roots[1].clone(), roots[1].clone()]);
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
            &asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups::from_roots(
                &lower_promql_roots(&workload).unwrap(),
            ),
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
        let summary: Vec<_> = compose_logical_candidate(&inventory, &choice).unwrap();
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
