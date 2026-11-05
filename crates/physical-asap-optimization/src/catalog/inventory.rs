//! Lower the logical candidate memo into one operator node per candidate,
//! and the candidates that may produce each of their inputs: the input
//! group's candidates whose logical properties satisfy the input's
//! requirement (`α^log`).
//!
//! Consumer-dependent demand, composed accuracy, and physical materialization
//! are handled by later configuration and evaluation stages. Nothing is
//! priced.

use std::rc::Rc;

use asap_accuracy::propagation::composed_guarantee;
use asap_types::ir::operator::AggIntent;
use asap_types::ir::{NonASAPOp, OperatorNode};
use asap_types::stages::{
    satisfies, CandidateForMemoGroupId, Memo, MemoGroupId, OperatorNodeOfCandidateForMemoGroup,
    PhysicalCandidateId, ProducerCandidates, ProducerSetInterner,
};

/// The memo's candidates lowered to operator nodes, before timing and
/// lifetime. Node IDs index `operator_nodes_of_candidates_for_memo_groups`;
/// they become [`asap_types::stages::OperatorNodeOfCandidateForMemoGroupId`]s
/// of the configured candidates.
#[derive(Debug)]
pub struct CandidateInventory {
    /// One per memo candidate.
    pub operator_nodes_of_candidates_for_memo_groups: Vec<OperatorNodeOfCandidateForMemoGroup>,
    /// Per node, per input (in `map_children` order): the nodes that may
    /// produce it.
    pub producer_nodes_per_input: Vec<Vec<ProducerCandidates>>,
    /// Per output reference: the nodes of its group whose output matches the
    /// group's logical node.
    pub query_output_nodes: Vec<Vec<PhysicalCandidateId>>,
    /// Per group: the nodes of its candidates.
    pub nodes_of_group: Vec<Vec<PhysicalCandidateId>>,
}

impl CandidateInventory {
    /// Every producer and query output names an existing node, once per list.
    pub fn validate(&self) -> Result<(), String> {
        let count = self.operator_nodes_of_candidates_for_memo_groups.len();
        if self.producer_nodes_per_input.len() != count {
            return Err("producer lists are not indexed by node".into());
        }
        for (node, inputs) in self.producer_nodes_per_input.iter().enumerate() {
            for (input, producer_candidates) in inputs.iter().enumerate() {
                super::validate_ids(
                    producer_candidates,
                    count,
                    &format!("candidate {node}, requirement {input}"),
                )?;
            }
        }
        for (output, nodes) in self.query_output_nodes.iter().enumerate() {
            super::validate_ids(nodes, count, &format!("query root {output}"))?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CandidateInventoryError {
    #[error("invalid candidate memo: {0}")]
    InvalidMemo(String),
    #[error("unsupported candidate {candidate} in group {group}: {reason}")]
    Unsupported {
        group: usize,
        candidate: usize,
        reason: &'static str,
    },
    #[error(
        "physical operator implementation for candidate {candidate} in group {group}: {reason}"
    )]
    PhysicalImplementation {
        group: usize,
        candidate: usize,
        reason: String,
    },
}

/// The inventory of `memo`'s candidates: one node per candidate, each
/// input's producers the input group's candidates that satisfy its
/// requirement.
pub fn build_candidate_inventory(
    memo: &Memo,
    output_groups: &[MemoGroupId],
) -> Result<CandidateInventory, CandidateInventoryError> {
    let invalid = |message: &str| CandidateInventoryError::InvalidMemo(message.into());
    let group_count = memo.groups.len();
    if group_count != memo.candidates.len() {
        return Err(invalid("group and candidate vector lengths differ"));
    }
    if output_groups
        .iter()
        .any(|group| group.0 as usize >= group_count)
    {
        return Err(invalid("query output references missing group"));
    }
    let mut inventory = CandidateInventory {
        operator_nodes_of_candidates_for_memo_groups: Vec::new(),
        producer_nodes_per_input: Vec::new(),
        query_output_nodes: Vec::new(),
        nodes_of_group: vec![Vec::new(); group_count],
    };
    let mut input_groups = Vec::new();
    for (group, candidates) in memo.candidates.iter().enumerate() {
        for (index, candidate) in candidates.iter().enumerate() {
            let unsupported = |reason| CandidateInventoryError::Unsupported {
                group,
                candidate: index,
                reason,
            };
            let mut implemented = vec![MemoGroupId(group as u32)];
            implemented.extend(&candidate.fused_groups);
            implemented.sort_unstable();
            implemented.dedup();
            if implemented.len() != 1 + candidate.fused_groups.len()
                || implemented.iter().any(|id| id.0 as usize >= group_count)
            {
                return Err(invalid(
                    "candidate names duplicate or missing absorbed groups",
                ));
            }
            if candidate.accuracy_budget_parts == 0 {
                return Err(unsupported("zero accuracy budget parts"));
            }
            let mut inputs = Vec::new();
            candidate
                .expression
                .operator
                .map_children(|&child| inputs.push(child));
            if inputs.iter().any(|child| child.0 as usize >= group_count) {
                return Err(invalid("candidate references missing input group"));
            }
            if inputs.len() != candidate.input_requirements.len() {
                return Err(invalid("candidate has not one requirement per input"));
            }
            let id = CandidateForMemoGroupId {
                group: MemoGroupId(group as u32),
                index,
            };
            let implementation_error = |reason: String| CandidateInventoryError::PhysicalImplementation {
                group,
                candidate: index,
                reason,
            };
            let node = memo
                .operator_node_of(id)
                .map_err(|error| implementation_error(error.to_string()))?;
            if let Some(NonASAPOp::Aggregate { measures, child, .. }) = node.non_asap() {
                if measures
                    .iter()
                    .any(|measure| matches!(measure, AggIntent::TopK { .. }))
                {
                    let reason = asap_types::ir::schema::topk::exact_topk_as_sorted_limit(
                        &node,
                        Rc::clone(child),
                    )
                    .err()
                    .unwrap_or_else(|| "exact top-k not as Sort and Limit".into());
                    return Err(implementation_error(reason));
                }
            }
            // A node other than its group's own carries the guarantee its
            // inputs compose.
            let node = if node.guarantee.is_none() && !Rc::ptr_eq(&node, &memo.groups[group].logical_node) {
                let guarantee = composed_guarantee(&node);
                Rc::new((*node).clone().with_guarantee(guarantee))
            } else {
                node
            };
            let node_id =
                PhysicalCandidateId(inventory.operator_nodes_of_candidates_for_memo_groups.len());
            inventory.nodes_of_group[group].push(node_id);
            inventory.operator_nodes_of_candidates_for_memo_groups.push(
                OperatorNodeOfCandidateForMemoGroup {
                    candidate_for_memo_group: id,
                    accuracy_budget_parts: candidate.accuracy_budget_parts,
                    physical_node: node,
                },
            );
            input_groups.push(inputs);
        }
    }
    let mut interner = ProducerSetInterner::default();
    for (node, inputs) in input_groups.iter().enumerate() {
        let consumer = memo.candidate(
            inventory.operator_nodes_of_candidates_for_memo_groups[node].candidate_for_memo_group,
        );
        let producers = inputs
            .iter()
            .zip(&consumer.input_requirements)
            .map(|(input, requirement)| {
                let satisfying: Vec<PhysicalCandidateId> = inventory.nodes_of_group
                    [input.0 as usize]
                    .iter()
                    .copied()
                    .filter(|producer| {
                        let id = inventory.operator_nodes_of_candidates_for_memo_groups
                            [producer.0]
                            .candidate_for_memo_group;
                        satisfies(&memo.candidate(id).logical_properties, requirement)
                    })
                    .collect();
                interner.intern(satisfying)
            })
            .collect();
        inventory.producer_nodes_per_input.push(producers);
    }
    for group in output_groups {
        let expected = &memo.groups[group.0 as usize].logical_node;
        let mut compatible_candidates = Vec::new();
        for &id in &inventory.nodes_of_group[group.0 as usize] {
            let node = &inventory.operator_nodes_of_candidates_for_memo_groups[id.0].physical_node;
            if compatible_output(expected, node)? {
                compatible_candidates.push(id);
            }
        }
        inventory.query_output_nodes.push(compatible_candidates);
    }
    inventory.validate().map_err(|error| invalid(&error))?;
    Ok(inventory)
}

fn compatible_output(
    expected: &OperatorNode,
    producer: &OperatorNode,
) -> Result<bool, CandidateInventoryError> {
    super::reconstruction::output_contract_satisfied(expected, producer)
        .map_err(CandidateInventoryError::InvalidMemo)
}
