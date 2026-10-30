//! Apply an explicit selected materialization assignment through real Stage 2.
//!
//! No fallback modes are chosen. Physical rewrites or timing splits that change
//! candidate identity are rejected until represented by physical enumeration.
//! This validates execution legality, not cost estimates or accuracy targets.
use std::collections::HashMap;
use std::rc::Rc;

use crate::{optimize_physical_asap_chosen, PhysicalWorkloadPlan};
use asap_types::ir::{OperatorNode, QueryRoot};
use asap_types::physical::Materialization;
use asap_types::workload::{DataWorkload, QueryWorkloadEntry};

use asap_types::stages::{PhysicalASAPMemoCandidates, SelectedCandidateGraph};

use super::reconstruction::{
    output_contract_satisfied, paired_node_identities, reconstruct_selected_candidates,
};

/// Only rejected assignments are safe to exclude from the finite search.
/// Adapter/identity failures must stop with diagnostics, not become no-good cuts.
#[derive(Debug, thiserror::Error)]
pub enum PhysicalCandidateError {
    #[error("selected physical assignment is infeasible: {0}")]
    RejectedAssignment(String),
    #[error("invalid or unsupported physical reconstruction: {0}")]
    InvalidOrUnsupported(String),
}

impl From<String> for PhysicalCandidateError {
    fn from(message: String) -> Self {
        Self::InvalidOrUnsupported(message)
    }
}
impl From<&str> for PhysicalCandidateError {
    fn from(message: &str) -> Self {
        Self::InvalidOrUnsupported(message.into())
    }
}

pub struct SelectedPhysicalCandidate {
    pub physical: PhysicalWorkloadPlan,
    pub nodes_by_candidate: Vec<Option<Rc<OperatorNode>>>,
}

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
