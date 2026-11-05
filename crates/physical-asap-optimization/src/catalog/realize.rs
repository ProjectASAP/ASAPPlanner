//! Apply an explicit selected physical properties assignment through real Stage 2.
//!
//! No fallback modes are chosen. Physical rewrites or timing splits that change
//! candidate identity are rejected until represented by physical enumeration.
//! This validates execution legality, not cost estimates or accuracy targets.
use asap_types::physical::PhysicalProperties;
use std::collections::HashMap;
use std::rc::Rc;

use crate::optimize_physical_asap_chosen;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use asap_types::stages::{
    AssignmentRealizer, DagOfSelectedCandidates, PhysicalASAPMemoCandidates,
    PhysicalCandidateError, RealizedPlan,
};

use super::reconstruction::{
    output_contract_satisfied, paired_node_identities, reconstruct_selected_candidates,
};

/// Realize `selected` with each selected node at its candidate's
/// physical properties. Output
/// references must follow `map_operator_refs` traversal of the original
/// roots in workload order (one reference for an operator root). Repeated
/// scalar references remain separate positions; literal-only scalar roots
/// use none.
pub fn realize_selected_candidates(
    physical_candidates: &PhysicalASAPMemoCandidates,
    selected: &DagOfSelectedCandidates,
    original_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
) -> Result<RealizedPlan, PhysicalCandidateError> {
    if entries.len() != original_roots.len() {
        return Err("workload entries and query roots differ in length".into());
    }
    let rebuilt = reconstruct_selected_candidates(physical_candidates, selected)?;
    // `reconstruct_selected_candidates` checked every selected candidate exists.
    // Each selected node's physical properties.
    let assignments: Vec<PhysicalProperties> = selected
        .candidate_of_node
        .iter()
        .map(|id| physical_candidates.candidates[id.0].properties)
        .collect();
    let mut node_by_pointer = HashMap::new();
    for (node, operator_node) in rebuilt.operator_node_of_selected_node.iter().enumerate() {
        node_by_pointer.insert(Rc::as_ptr(operator_node), node);
    }
    let roots = restore_query_roots(original_roots, &rebuilt.outputs)?;
    let mut assignment_error = None;
    let mut seen = vec![false; assignments.len()];
    let physical = optimize_physical_asap_chosen(
        "selected-candidate-connections",
        &roots,
        entries,
        data,
        query_time_results_retained,
        &mut |logical, _, options| {
            let node = node_by_pointer.get(&logical).copied();
            // A node outside the selection is not kept.
            let requested = node.map_or(PhysicalProperties::NOT_KEPT_AT_QUERY_TIME, |node| {
                seen[node] = true;
                assignments[node]
            });
            // The option keeping it as requested; option `None` (not kept)
            // for a requested lifetime `None`, at either timing.
            let chosen = options.iter().position(|option| match option {
                Some(kept) => *kept == requested,
                None => !requested.lifetime().is_kept(),
            });
            if chosen.is_none() {
                let message = format!(
                    "selected properties {requested:?} are not offered for selected node {node:?}"
                );
                assignment_error = Some(if node.is_some() {
                    PhysicalCandidateError::RejectedAssignment(message)
                } else {
                    PhysicalCandidateError::InvalidOrUnsupported(message)
                });
            }
            chosen
        },
    )
    .map_err(|error| error.to_string())?;
    if let Some(error) = assignment_error {
        return Err(error);
    }
    let physical = physical.ok_or_else(|| {
        PhysicalCandidateError::RejectedAssignment(
            "selected physical properties assignment violates Stage 2 timing or dependency rules"
                .into(),
        )
    })?;
    physical.dag.validate().map_err(|error| error.to_string())?;
    let physical_outputs = ordered_output_references(&physical.roots);
    let identities = paired_node_identities(&rebuilt.outputs, &physical_outputs)?;
    for (node, properties) in assignments.iter().enumerate() {
        if properties.lifetime().is_kept() && !seen[node] {
            // The decoder already established node identity. Stage 2 did
            // not offer this selected node (e.g. a lower shared non-summary),
            // so the assignment is infeasible rather than an adapter failure.
            return Err(PhysicalCandidateError::RejectedAssignment(format!(
                "selected node {node} is not an eligible kept-output node in the selected graph"
            )));
        }
    }
    let operator_node_of_selected_node = rebuilt
        .operator_node_of_selected_node
        .into_iter()
        .enumerate()
        .map(|(node, operator_node)| {
            let physical = identities
                .get(&Rc::as_ptr(&operator_node))
                .cloned()
                .ok_or_else(|| format!("selected node {node} has no physical identity"))?;
            if physical.lifetime != assignments[node].lifetime() {
                return Err(format!(
                    "selected node {node} lifetime changed during physical reconstruction"
                ));
            }
            Ok(physical)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let range_evaluations: Vec<_> = entries.iter().map(|entry| entry.range_evaluation).collect();
    let tumbling_window_retention_ms =
        asap_cost::tumbling_window_retention_ms_of_plan(&physical.dag, &range_evaluations);
    Ok(RealizedPlan {
        plan: physical,
        tumbling_window_retention_ms,
        operator_node_of_selected_node,
    })
}

/// Stage 2's [`AssignmentRealizer`], over the planning context: the
/// workload's original query roots (one per entry of `entries`), its data,
/// and whether the deployment keeps query-time results across
/// evaluations. The facade builds it, passes it to Stage 3, and builds the
/// selected plan with it.
#[derive(Debug, Clone, Copy)]
pub struct Stage2AssignmentRealizer<'a> {
    pub original_roots: &'a [QueryRoot],
    pub entries: &'a [QueryWorkloadEntry],
    pub data: &'a DataWorkload,
    pub query_time_results_retained: bool,
}

impl AssignmentRealizer for Stage2AssignmentRealizer<'_> {
    /// `realize_selected_candidates`; each node also keeps its
    /// candidate's timing.
    fn realize(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &DagOfSelectedCandidates,
    ) -> Result<RealizedPlan, PhysicalCandidateError> {
        let realized = realize_selected_candidates(
            physical,
            selected,
            self.original_roots,
            self.entries,
            self.data,
            self.query_time_results_retained,
        )?;
        for (id, node) in selected
            .candidate_of_node
            .iter()
            .zip(&realized.operator_node_of_selected_node)
        {
            let timing = physical.candidates[id.0].properties.timing();
            if node.timing != Some(timing) {
                return Err(PhysicalCandidateError::InvalidOrUnsupported(format!(
                    "candidate {} requested timing {timing:?}, Stage 2 produced {:?}",
                    id.0, node.timing
                )));
            }
        }
        Ok(realized)
    }
}

fn ordered_output_references(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
    asap_types::stages::output_references(roots)
        .into_iter()
        .map(|(_, node)| node)
        .collect()
}

pub fn restore_query_roots(
    original: &[QueryRoot],
    outputs: &[Rc<OperatorNode>],
) -> Result<Vec<QueryRoot>, String> {
    let references = ordered_output_references(original);
    if references.len() != outputs.len() {
        return Err("output references do not match original query-root mapping".into());
    }
    for (reference, output) in references.iter().zip(outputs) {
        if !output_contract_satisfied(reference, output)? {
            return Err(format!(
                "selected output reference has incompatible result or schema: expected {:?} {:?}, received {:?} {:?}",
                reference.result_kind, reference.schema, output.result_kind, output.schema
            ));
        }
    }
    let mut next = outputs.iter();
    Ok(original
        .iter()
        .map(|root| match root {
            QueryRoot::Operator(_) => QueryRoot::Operator(Rc::clone(next.next().unwrap())),
            QueryRoot::Scalar(expression) => QueryRoot::Scalar(
                expression.map_operator_refs(&mut |_| Rc::clone(next.next().unwrap())),
            ),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::generate_dag_of_logical_memo_groups;
    use asap_logical_asap_optimization::test_support::generate_per_computation_candidates;
    use asap_logical_asap_optimization::test_support::try_populate_pass1;
    use asap_logical_asap_optimization::test_support::{
        entry, lower_promql, sharing_rules_and_shared_state_groups,
    };
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::stages::{
        LogicalASAPMemoCandidates, PhysicalCandidateId, SelectedEdge, SelectedNodeId,
    };
    use asap_types::workload::QueryRecurrence;

    /// Add a node running `candidate` and, for each input, a node of its
    /// first producer that is not kept (else its first producer),
    /// recursively; returns the new node.
    fn select_from(
        physical: &PhysicalASAPMemoCandidates,
        candidate: PhysicalCandidateId,
        selected: &mut DagOfSelectedCandidates,
    ) -> SelectedNodeId {
        let node = SelectedNodeId(selected.candidate_of_node.len());
        selected.candidate_of_node.push(candidate);
        for (input, producer_candidates) in physical.candidates[candidate.0]
            .producer_candidates_per_input
            .iter()
            .enumerate()
        {
            let producer = producer_candidates
                .iter()
                .copied()
                .find(|s| !physical.candidates[s.0].properties.lifetime().is_kept())
                .unwrap_or(producer_candidates[0]);
            let producer = select_from(physical, producer, selected);
            selected.edges.push(SelectedEdge {
                producer,
                consumer: node,
                input,
            });
        }
        node
    }

    // The trait object returns what the catalog function returns with each
    // candidate's own physical properties.
    #[test]
    fn stage2_realizer_matches_direct_catalog_calls() {
        let root = QueryRoot::Operator(lower_promql(
            "sum by (job) (sum_over_time(m[1m]))",
            AccuracyTarget::exact(),
        ));
        let roots = vec![root.clone()];
        let initial = generate_dag_of_logical_memo_groups(&[root]);
        let per_computation =
            generate_per_computation_candidates(&generate_dag_of_logical_memo_groups(&[
                roots[0].clone()
            ]))
            .unwrap();
        let memo = try_populate_pass1(initial, &per_computation).unwrap();
        let logical = LogicalASAPMemoCandidates {
            memo: sharing_rules_and_shared_state_groups(memo).unwrap(),
        };
        let entries = vec![entry(QueryRecurrence::Unknown)];
        let data = DataWorkload::default();
        let physical = crate::catalog::generate_physical_asap_memo_candidates(
            &logical, &entries, &data, false, true,
        )
        .unwrap();
        let selected = physical
            .query_output_candidates(0)
            .map(|candidate| {
                let mut selected = DagOfSelectedCandidates {
                    candidate_of_node: Vec::new(),
                    edges: Vec::new(),
                    node_of_output_reference: Vec::new(),
                };
                let root = select_from(&physical, candidate, &mut selected);
                selected.node_of_output_reference.push(root);
                selected
            })
            .find(|selected| reconstruct_selected_candidates(&physical, selected).is_ok())
            .expect("some selection reconstructs");
        assert!(selected.candidate_of_node.len() > 1);
        let realizer = Stage2AssignmentRealizer {
            original_roots: &roots,
            entries: &entries,
            data: &data,
            query_time_results_retained: false,
        };
        let realizer: &dyn AssignmentRealizer = &realizer;

        let direct =
            realize_selected_candidates(&physical, &selected, &roots, &entries, &data, false)
                .unwrap();
        let through_trait = realizer.realize(&physical, &selected).unwrap();
        assert_eq!(
            format!("{:?}", direct.plan),
            format!("{:?}", through_trait.plan)
        );
        assert_eq!(
            direct.operator_node_of_selected_node,
            through_trait.operator_node_of_selected_node
        );
        assert_eq!(
            direct.tumbling_window_retention_ms,
            through_trait.tumbling_window_retention_ms
        );
    }
}
