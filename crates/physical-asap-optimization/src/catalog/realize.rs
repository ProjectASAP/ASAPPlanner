//! Apply an explicit selected materialization assignment through real Stage 2.
//!
//! No fallback modes are chosen. Physical rewrites or timing splits that change
//! candidate identity are rejected until represented by physical enumeration.
//! This validates execution legality, not cost estimates or accuracy targets.
use std::collections::HashMap;
use std::rc::Rc;

use crate::optimize_physical_asap_chosen;
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use asap_types::stages::{
    AssignmentRealizer, PhysicalASAPMemoCandidates, PhysicalCandidateError, SelectedCandidateGraph,
    SelectedPhysicalCandidate,
};

use super::reconstruction::{
    output_contract_satisfied, paired_node_identities, reconstruct_selected_candidates,
};

/// Output references must follow `map_operator_refs` traversal of the original
/// roots in workload order (one reference for an operator root). Repeated scalar
/// references remain separate positions; literal-only scalar roots use none.
pub fn materialize_selected_candidates(
    physical_candidates: &PhysicalASAPMemoCandidates,
    selected: &SelectedCandidateGraph,
    materializations: &[Option<Materialization>],
    original_roots: &[QueryRoot],
    entries: &[QueryWorkloadEntry],
    data: &DataWorkload,
    query_time_results_retained: bool,
) -> Result<SelectedPhysicalCandidate, PhysicalCandidateError> {
    if entries.len() != original_roots.len() {
        return Err("workload entries and query roots differ in length".into());
    }
    let rebuilt = reconstruct_selected_candidates(physical_candidates, selected)?;
    if materializations.len() != rebuilt.nodes_by_candidate.len() {
        return Err("materialization vector must be indexed by candidate ID".into());
    }
    let mut ids_by_pointer = HashMap::new();
    for (id, node) in rebuilt.nodes_by_candidate.iter().enumerate() {
        if node.is_some() != materializations[id].is_some() {
            return Err(format!(
                "candidate {id} activation and materialization assignment disagree"
            )
            .into());
        }
        if let Some(node) = node {
            ids_by_pointer.insert(Rc::as_ptr(node), id);
        }
    }
    let roots = restore_query_roots(original_roots, &rebuilt.outputs)?;
    let mut assignment_error = None;
    let mut seen = vec![false; materializations.len()];
    let physical = optimize_physical_asap_chosen(
        "selected-candidate-connections",
        &roots,
        entries,
        data,
        query_time_results_retained,
        &mut |logical, _, options| {
            let requested = if let Some(&id) = ids_by_pointer.get(&logical) {
                seen[id] = true;
                materializations[id].unwrap()
            } else {
                Materialization::NotMaterialized
            };
            if !options.contains(&requested) {
                let message = format!(
                    "selected materialization {requested:?} is not offered for candidate {:?}",
                    ids_by_pointer.get(&logical)
                );
                assignment_error = Some(if ids_by_pointer.contains_key(&logical) {
                    PhysicalCandidateError::RejectedAssignment(message)
                } else {
                    PhysicalCandidateError::InvalidOrUnsupported(message)
                });
            }
            requested
        },
    )
    .map_err(|error| error.to_string())?;
    if let Some(error) = assignment_error {
        return Err(error);
    }
    let physical = physical.ok_or_else(|| {
        PhysicalCandidateError::RejectedAssignment(
            "selected materialization assignment violates Stage 2 timing or dependency rules"
                .into(),
        )
    })?;
    physical.dag.validate().map_err(|error| error.to_string())?;
    let physical_outputs = ordered_output_references(&physical.roots);
    let identities = paired_node_identities(&rebuilt.outputs, &physical_outputs)?;
    for (id, mode) in materializations.iter().enumerate() {
        if mode.is_some_and(|mode| mode != Materialization::NotMaterialized) && !seen[id] {
            // The decoder already established candidate identity. Stage 2 did
            // not offer this selected node (e.g. a lower shared non-summary),
            // so the assignment is infeasible rather than an adapter failure.
            return Err(PhysicalCandidateError::RejectedAssignment(format!(
                "candidate {id} is not an eligible materialization node in the selected graph"
            )));
        }
    }
    let nodes_by_candidate = rebuilt
        .nodes_by_candidate
        .into_iter()
        .enumerate()
        .map(|(id, node)| {
            node.map(|node| {
                let physical = identities
                    .get(&Rc::as_ptr(&node))
                    .cloned()
                    .ok_or_else(|| format!("candidate {id} has no physical identity"))?;
                if Some(physical.materialization) != materializations[id] {
                    return Err(format!(
                        "candidate {id} materialization changed during physical reconstruction"
                    ));
                }
                Ok(physical)
            })
            .transpose()
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(SelectedPhysicalCandidate {
        physical,
        nodes_by_candidate,
    })
}

/// Stage 2's [`AssignmentRealizer`]: the facade passes it to Stage 3. Each
/// method delegates to the catalog function of the same operation.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stage2AssignmentRealizer;

impl AssignmentRealizer for Stage2AssignmentRealizer {
    fn reconstruct_selected_outputs(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &SelectedCandidateGraph,
    ) -> Result<Vec<Rc<OperatorNode>>, String> {
        Ok(reconstruct_selected_candidates(physical, selected)?.outputs)
    }

    fn realize_selected_assignment(
        &self,
        physical: &PhysicalASAPMemoCandidates,
        selected: &SelectedCandidateGraph,
        materializations: &[Option<Materialization>],
        original_roots: &[QueryRoot],
        entries: &[QueryWorkloadEntry],
        data: &DataWorkload,
        query_time_results_retained: bool,
    ) -> Result<SelectedPhysicalCandidate, PhysicalCandidateError> {
        materialize_selected_candidates(
            physical,
            selected,
            materializations,
            original_roots,
            entries,
            data,
            query_time_results_retained,
        )
    }
}

pub fn ordered_output_references(roots: &[QueryRoot]) -> Vec<Rc<OperatorNode>> {
    let mut references = Vec::new();
    for root in roots {
        match root {
            QueryRoot::Operator(node) => references.push(Rc::clone(node)),
            QueryRoot::Scalar(expression) => {
                expression.map_operator_refs(&mut |node| {
                    references.push(Rc::clone(node));
                    Rc::clone(node)
                });
            }
        }
    }
    references
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
    use asap_logical_asap_optimization::pass1::per_computation_candidates::generate_per_computation_candidates;
    use asap_logical_asap_optimization::pass2::memo::candidates::try_populate_pass1;
    use asap_logical_asap_optimization::pass2::memo::shared_configurations::PaneWidthDomain;
    use asap_logical_asap_optimization::pass2::memo::DagOfLogicalMemoGroups;
    use asap_logical_asap_optimization::test_support::{
        entry, lower_promql, sharing_rules_and_shared_state_groups,
    };
    use asap_types::ir::properties::AccuracyTarget;
    use asap_types::stages::{
        LogicalASAPMemoCandidates, PhysicalCandidateId, ProducerConsumerConnection,
        QueryRootAssignment,
    };
    use asap_types::workload::QueryRecurrence;

    /// Select `candidate` and, for each input, its first unmaterialized
    /// supplier (else its first supplier), recursively.
    fn select_from(
        physical: &PhysicalASAPMemoCandidates,
        candidate: PhysicalCandidateId,
        selected: &mut SelectedCandidateGraph,
    ) {
        if selected.selected_candidates.contains(&candidate) {
            return;
        }
        selected.selected_candidates.push(candidate);
        for (requirement, suppliers) in physical.candidates[candidate.0]
            .supplier_candidates_per_input
            .iter()
            .enumerate()
        {
            let producer = suppliers
                .iter()
                .copied()
                .find(|s| {
                    physical.candidates[s.0].materialization == Materialization::NotMaterialized
                })
                .unwrap_or(suppliers[0]);
            selected.connections.push(ProducerConsumerConnection {
                producer,
                consumer: candidate,
                requirement,
            });
            select_from(physical, producer, selected);
        }
    }

    // The trait object returns exactly what the catalog functions return.
    #[test]
    fn stage2_realizer_matches_direct_catalog_calls() {
        let root = QueryRoot::Operator(lower_promql(
            "sum by (job) (sum_over_time(m[1m]))",
            AccuracyTarget::exact(),
        ));
        let roots = vec![root.clone()];
        let initial = DagOfLogicalMemoGroups::from_roots(&[(0, root)]);
        let per_computation =
            generate_per_computation_candidates(vec![(0, roots[0].clone())]).unwrap();
        let memo = try_populate_pass1(initial, &per_computation).unwrap();
        let logical = LogicalASAPMemoCandidates {
            memo: sharing_rules_and_shared_state_groups(memo, PaneWidthDomain::Complete).unwrap(),
        };
        let entries = vec![entry(QueryRecurrence::Unknown)];
        let data = DataWorkload::default();
        let physical = crate::catalog::generate_physical_asap_memo_candidates(
            &logical, &entries, &data, false, true, None,
        )
        .unwrap();
        let selected = physical
            .query_output_candidates(0)
            .map(|candidate| {
                let mut selected = SelectedCandidateGraph {
                    selected_candidates: Vec::new(),
                    connections: Vec::new(),
                    query_roots: vec![QueryRootAssignment {
                        query_root: 0,
                        candidate,
                    }],
                    modeled_cost: 0.0,
                };
                select_from(&physical, candidate, &mut selected);
                selected
            })
            .find(|selected| reconstruct_selected_candidates(&physical, selected).is_ok())
            .expect("some selection reconstructs");
        assert!(selected.selected_candidates.len() > 1);
        let realizer: &dyn AssignmentRealizer = &Stage2AssignmentRealizer;

        let direct = reconstruct_selected_candidates(&physical, &selected)
            .unwrap()
            .outputs;
        let through_trait = realizer
            .reconstruct_selected_outputs(&physical, &selected)
            .unwrap();
        assert_eq!(direct, through_trait);

        let mut materializations = vec![None; physical.candidates.len()];
        for id in &selected.selected_candidates {
            materializations[id.0] = Some(physical.candidates[id.0].materialization);
        }
        let direct = materialize_selected_candidates(
            &physical,
            &selected,
            &materializations,
            &roots,
            &entries,
            &data,
            false,
        )
        .unwrap();
        let through_trait = realizer
            .realize_selected_assignment(
                &physical,
                &selected,
                &materializations,
                &roots,
                &entries,
                &data,
                false,
            )
            .unwrap();
        assert_eq!(
            format!("{:?}", direct.physical),
            format!("{:?}", through_trait.physical)
        );
        assert_eq!(direct.nodes_by_candidate, through_trait.nodes_by_candidate);

        // Errors pass through unchanged too.
        let too_short = &materializations[..materializations.len() - 1];
        let direct = materialize_selected_candidates(
            &physical, &selected, too_short, &roots, &entries, &data, false,
        )
        .err()
        .unwrap();
        let through_trait = realizer
            .realize_selected_assignment(
                &physical, &selected, too_short, &roots, &entries, &data, false,
            )
            .err()
            .unwrap();
        assert_eq!(direct.to_string(), through_trait.to_string());
    }
}
