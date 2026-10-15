//! Display labels for the planning stages' candidates, for the
//! `stage_pipeline` viewer document only. Planning never reads them: they
//! are pure functions of the candidates' planning data (the Pass 1 choice,
//! the Pass 2 rules applied and the materialization assignment).

use std::collections::BTreeMap;
use std::rc::Rc;

use asap_aware_mapping::pass::Pass2Candidate;
use asap_aware_mapping::per_computation_candidates::{
    ComputationCandidate, PerComputationCandidates,
};
use asap_aware_mapping::stage2_physical_asap_optimization::PhysicalCandidate;
use asap_types::ir::{ASAPOp, Operator, OperatorNode, QueryRoot};
use asap_types::post_asap::{FieldDataType, Materialization, SketchAlgorithm};

use crate::planning_stages::PlanningStagesRun;

/// The label of every logical (`L<n>`) and physical (`P<n>`, `P<n>-m<k>`)
/// candidate of `run`, by id.
pub fn candidate_labels(run: &PlanningStagesRun) -> BTreeMap<String, String> {
    let pass2: BTreeMap<&str, &Pass2Candidate> = run
        .stage1_pass2
        .iter()
        .map(|p| (p.id.as_str(), p))
        .collect();
    let mut labels = BTreeMap::new();
    for candidate in &run.stage1_logical_asap_optimization {
        let mut label = choice_label(&run.stage1_pass1_per_computation, &candidate.choice);
        if let Some(p) = pass2.get(candidate.id.as_str()) {
            label += &pass2_label(p);
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

/// A Pass 1 choice, e.g. "Q1 exact · Q2 CMS+heap" (exact accumulators in
/// parentheses), each target listed under the first query that reads it.
pub fn choice_label<Id>(inventory: &PerComputationCandidates<Id>, choice: &[usize]) -> String {
    let query_of_target: Vec<usize> = inventory
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
                            .any(|n| Rc::ptr_eq(n, &target.target))
                    })
                })
                .expect("every target is reached from a root")
        })
        .collect();
    // Targets a chosen whole-expression candidate absorbs are not computed.
    let absorbed: Vec<usize> = inventory
        .targets
        .iter()
        .zip(choice)
        .filter(|(target, &index)| {
            matches!(
                target.choices.candidates[index],
                ComputationCandidate::WholeExpression(_)
            )
        })
        .filter_map(|(target, _)| target.absorbs)
        .collect();
    (0..inventory.roots.len())
        .map(|query| {
            let mut sketches = Vec::new();
            let mut accumulators = Vec::new();
            for (t, (target, (&target_query, &index))) in inventory
                .targets
                .iter()
                .zip(query_of_target.iter().zip(choice))
                .enumerate()
            {
                if target_query != query || absorbed.contains(&t) {
                    continue;
                }
                match &target.choices.candidates[index] {
                    ComputationCandidate::Exact => {}
                    ComputationCandidate::ExactAccumulator { kind, .. } => {
                        accumulators.push(format!("{kind:?} acc"))
                    }
                    ComputationCandidate::AvgAsSumCount => {
                        accumulators.push("Sum/Count acc".into())
                    }
                    ComputationCandidate::Summary(algorithm)
                    | ComputationCandidate::WholeExpression(algorithm) => {
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
/// " · one UnivMon for Q1, Q3 · Kll for Q1 in 5 × 1m panes · shared Scan".
pub fn pass2_label(pass2: &Pass2Candidate) -> String {
    let mut label = String::new();
    if !pass2.summaries.is_empty() {
        label += &format!(" · one {}", pass2.summaries.join(", one "));
    }
    if !pass2.panes.is_empty() {
        label += &format!(" · {}", pass2.panes.join(", "));
    }
    if !pass2.shared.is_empty() {
        label += &format!(" · shared {}", pass2.shared.join(", "));
    }
    label
}

/// `logical` followed by what the candidate materializes, e.g.
/// "… · ingestion time: Kll ×5 panes · query time, shared: DDSketch";
/// just `logical` when nothing is.
pub fn physical_label(logical: &str, candidate: &PhysicalCandidate) -> String {
    let mut parts: Vec<(&str, Vec<String>)> = Vec::new();
    for (id, materialization) in &candidate.materialization {
        let phase = match materialization {
            Materialization::NotMaterialized => continue,
            Materialization::MatAtIngestion(_) => "ingestion time",
            Materialization::MatAtQuery(_) => "query time, shared",
        };
        let node = candidate
            .dag
            .nodes
            .iter()
            .find(|n| n.id == *id)
            .expect("an assigned node is in the DAG");
        let label = build_label(&node.payload);
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
    let Operator::ASAP(op) = payload else {
        return payload.kind_name().to_string();
    };
    let state = match op {
        ASAPOp::SummaryAgg { family, .. } | ASAPOp::PaneChain { family, .. } => match family {
            FieldDataType::Sketch(kind, _) => sketch_name(kind.algorithm()),
            FieldDataType::ExactAggregate(kind, _) => format!("{kind:?} acc"),
            _ => payload.kind_name().to_string(),
        },
        _ => payload.kind_name().to_string(),
    };
    match op {
        ASAPOp::PaneChain { pane_count, .. } => format!("{state} ×{pane_count} panes"),
        _ => state,
    }
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
    use crate::planner_layering_examples::{example1, example2, example2_catalog};
    use crate::planning_stages::{lower_promql_roots, lower_sql_roots, run_planning_stages};

    /// Logical labels name each query's choice and the Pass 2 rules applied;
    /// a physical label adds what it materializes, and with nothing
    /// materialized it is its logical candidate's label.
    #[test]
    fn example1_labels_name_choices_rules_and_materialization() {
        let workload = example1();
        let run =
            run_planning_stages(&workload, lower_promql_roots(&workload).unwrap(), None).unwrap();
        let labels = candidate_labels(&run);
        assert_eq!(
            labels["L1"],
            "Q1 exact (Sum acc, Rate acc) · Q2 exact (Sum acc)"
        );
        assert!(labels.values().any(|l| l.ends_with(" · shared Scan #1")));
        assert!(labels
            .values()
            .any(|l| l.contains("Rate acc for Q1 in 6 × 10s panes")));
        for p in &run.stage2_physical_asap_optimization {
            let label = &labels[&p.id];
            assert!(label.starts_with(&labels[&p.from_logical]), "{label}");
            assert_eq!(
                p.materializes_any(),
                label != &labels[&p.from_logical],
                "{label}"
            );
        }
        assert!(labels
            .values()
            .any(|l| l.ends_with(" · ingestion time: Rate acc ×6 panes")));
        assert!(labels
            .values()
            .any(|l| l.ends_with(" · ingestion time: Sum acc ×6 panes")));
    }

    /// A summary build read by two queries can be built once per evaluation
    /// for both: its label says "query time, shared".
    #[test]
    fn example2_labels_name_builds_shared_at_query_time() {
        let workload = example2();
        let roots = lower_sql_roots(&workload, &example2_catalog()).unwrap();
        let run = run_planning_stages(&workload, roots, None).unwrap();
        let labels = candidate_labels(&run);
        assert!(
            labels
                .values()
                .any(|l| l.contains(" · query time, shared: UnivMon")),
            "{:?}",
            labels
                .values()
                .filter(|l| l.contains("query time"))
                .take(3)
                .collect::<Vec<_>>()
        );
    }
}
