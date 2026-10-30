//! Lower the logical candidate memo into typed immediate operator nodes and
//! the nodes that may produce each of their inputs.
//!
//! Consumer-dependent demand, composed accuracy, and physical materialization
//! are handled by later configuration and evaluation stages. Private exact
//! operators are lowered before decomposition; the resulting nodes still need
//! Stage 2 validation before execution. Nothing is priced.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::physical::Materialization;
use asap_types::stages::CandidateMemo;
use asap_types::stages::MemoGroupId;

use super::factoring::{MemoGroupOutputKey, MemoGroupOutputToCompatibleProducersCache};
use asap_types::stages::{
    CandidateForMemoGroupId, OperatorNodeOfCandidateForMemoGroup, PhysicalCandidateId,
    ProducerCandidates,
};

/// The memo's implementation candidates lowered to operator nodes, before
/// timing and materialization. Node IDs index
/// `operator_nodes_of_candidates_for_memo_groups`; they become
/// [`asap_types::stages::OperatorNodeOfCandidateForMemoGroupId`]s of the
/// configured candidates.
#[derive(Debug)]
pub struct CandidateInventory {
    /// One per operator node: each implementation root, then the private
    /// nodes below it.
    pub operator_nodes_of_candidates_for_memo_groups: Vec<OperatorNodeOfCandidateForMemoGroup>,
    /// Per node, per input (in `map_children` order): the nodes that may
    /// produce it (a private child, or the input group's compatible roots).
    pub producer_nodes_per_input: Vec<Vec<ProducerCandidates>>,
    /// Per output reference: the roots of its group whose output matches the
    /// group's logical node.
    pub query_output_nodes: Vec<Vec<PhysicalCandidateId>>,
    /// Per group: its implementation roots, excluding each implementation's
    /// private nodes.
    pub implementation_roots_of_group: Vec<Vec<PhysicalCandidateId>>,
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
    #[error(transparent)]
    DeadlineExceeded(#[from] asap_types::stages::CandidateGenerationTimeout),
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

/// The inventory with identical terminal readers factored into one node
/// (whose `candidates_for_memo_group` lists every candidate it can implement).
pub fn build_candidate_inventory_before(
    memo: &CandidateMemo,
    output_groups: &[MemoGroupId],
    deadline: Option<std::time::Instant>,
) -> Result<CandidateInventory, CandidateInventoryError> {
    let mut inventory = build_with_compatible_producers_cache(
        memo,
        output_groups,
        deadline,
        &mut MemoGroupOutputToCompatibleProducersCache::default(),
    )?;
    super::factoring::factor_terminal_readers(&mut inventory, deadline)?;
    Ok(inventory)
}

pub fn build_with_compatible_producers_cache(
    memo: &CandidateMemo,
    output_groups: &[MemoGroupId],
    deadline: Option<std::time::Instant>,
    compatible_producers_cache: &mut MemoGroupOutputToCompatibleProducersCache,
) -> Result<CandidateInventory, CandidateInventoryError> {
    super::check_deadline(deadline)?;
    let invalid = |message: &str| CandidateInventoryError::InvalidMemo(message.into());
    if memo.groups.len() != memo.candidates.len() {
        return Err(invalid("group and candidate vector lengths differ"));
    }
    if output_groups
        .iter()
        .any(|group| group.0 as usize >= memo.groups.len())
    {
        return Err(invalid("query output references missing group"));
    }
    let mut inventory = CandidateInventory {
        operator_nodes_of_candidates_for_memo_groups: Vec::new(),
        producer_nodes_per_input: Vec::new(),
        query_output_nodes: Vec::new(),
        implementation_roots_of_group: vec![Vec::new(); memo.groups.len()],
    };
    for (group, candidates) in memo.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        for (index, candidate) in candidates.iter().enumerate() {
            super::check_deadline(deadline)?;
            let unsupported = |reason| CandidateInventoryError::Unsupported {
                group,
                candidate: index,
                reason,
            };
            let implemented: HashSet<_> = std::iter::once(MemoGroupId(group as u32))
                .chain(candidate.fused_groups.iter().copied())
                .collect();
            if implemented.len() != 1 + candidate.fused_groups.len()
                || implemented
                    .iter()
                    .any(|id| id.0 as usize >= memo.groups.len())
            {
                return Err(invalid(
                    "candidate names duplicate or missing absorbed groups",
                ));
            }
            // Only the owner group is this candidate's output contract. Its fused
            // groups describe absorbed computations inside its private subtree,
            // not additional results it can produce. Private-node decomposition
            // charges that work; independent consumers of an absorbed group
            // still select one of that group's own candidates. Reverse support
            // leaves unused absorbed groups inactive without exclusion rules.
            if candidate.accuracy_budget_parts == 0 {
                return Err(unsupported("zero accuracy budget share"));
            }
            if candidate
                .input_groups
                .iter()
                .any(|child| child.0 as usize >= memo.groups.len())
            {
                return Err(invalid("candidate references missing input group"));
            }
            let inputs = candidate
                .input_groups
                .iter()
                .map(|child| Rc::clone(&memo.groups[child.0 as usize].logical_node))
                .collect::<Vec<_>>();
            if inputs
                .iter()
                .any(|input| Rc::ptr_eq(input, &candidate.root_node))
            {
                return Err(unsupported(
                    "root aliases an input group without an operator",
                ));
            }
            let implemented = crate::implement_candidate_operators(&candidate.root_node, &inputs)
                .map_err(|error| CandidateInventoryError::PhysicalImplementation {
                group,
                candidate: index,
                reason: error.to_string(),
            })?;
            let id =
                PhysicalCandidateId(inventory.operator_nodes_of_candidates_for_memo_groups.len());
            inventory.implementation_roots_of_group[group].push(id);
            inventory.operator_nodes_of_candidates_for_memo_groups.push(
                OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![CandidateForMemoGroupId {
                        group: MemoGroupId(group as u32),
                        index,
                    }],
                    is_implementation_root: true,
                    accuracy_budget_parts: candidate.accuracy_budget_parts,
                    physical_node: implemented,
                },
            );
            inventory.producer_nodes_per_input.push(Vec::new());
        }
    }
    for (group, candidates) in memo.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        for (index, candidate) in candidates.iter().enumerate() {
            super::check_deadline(deadline)?;
            let mut boundaries = HashMap::new();
            for &child in &candidate.input_groups {
                let pointer = Rc::as_ptr(&memo.groups[child.0 as usize].logical_node);
                if boundaries
                    .insert(pointer, child)
                    .is_some_and(|previous| previous != child)
                {
                    return Err(invalid("one boundary node names different input groups"));
                }
            }
            if boundaries.contains_key(&Rc::as_ptr(&candidate.root_node)) {
                return Err(CandidateInventoryError::Unsupported {
                    group,
                    candidate: index,
                    reason: "root aliases an input group without an operator",
                });
            }
            let mut builder = ImmediateCandidateBuilder {
                inventory: &mut inventory,
                compatible_producers_cache: &mut *compatible_producers_cache,
                boundaries,
                private_nodes: HashMap::new(),
                used_boundaries: HashSet::new(),
                owner: CandidateForMemoGroupId {
                    group: MemoGroupId(group as u32),
                    index,
                },
                owner_accuracy_budget_parts: candidate.accuracy_budget_parts,
                deadline,
            };
            let root_id = builder.inventory.implementation_roots_of_group[group][index];
            let implemented = Rc::clone(
                &builder
                    .inventory
                    .operator_nodes_of_candidates_for_memo_groups[root_id.0]
                    .physical_node,
            );
            builder.add(&implemented, Some(root_id))?;
            if candidate
                .input_groups
                .iter()
                .any(|group| !builder.used_boundaries.contains(group))
            {
                return Err(invalid(
                    "declared input group has no canonical node boundary",
                ));
            }
        }
    }
    for group in output_groups {
        let expected = &memo.groups[group.0 as usize].logical_node;
        let mut compatible_candidates = Vec::new();
        for &id in &inventory.implementation_roots_of_group[group.0 as usize] {
            let template =
                &inventory.operator_nodes_of_candidates_for_memo_groups[id.0].physical_node;
            if compatible_output(expected, template)? {
                compatible_candidates.push(id);
            }
        }
        inventory.query_output_nodes.push(compatible_candidates);
    }
    inventory.validate().map_err(|error| invalid(&error))?;
    super::check_deadline(deadline)?;
    Ok(inventory)
}

struct ImmediateCandidateBuilder<'a> {
    inventory: &'a mut CandidateInventory,
    compatible_producers_cache: &'a mut MemoGroupOutputToCompatibleProducersCache,
    boundaries: HashMap<*const OperatorNode, MemoGroupId>,
    private_nodes: HashMap<*const OperatorNode, PhysicalCandidateId>,
    used_boundaries: HashSet<MemoGroupId>,
    /// The memo candidate being lowered; owner of every node it adds.
    owner: CandidateForMemoGroupId,
    /// The owner's `accuracy_budget_parts`, which its private nodes carry.
    owner_accuracy_budget_parts: usize,
    deadline: Option<std::time::Instant>,
}

impl ImmediateCandidateBuilder<'_> {
    fn add(
        &mut self,
        node: &Rc<OperatorNode>,
        reserved: Option<PhysicalCandidateId>,
    ) -> Result<PhysicalCandidateId, CandidateInventoryError> {
        super::check_deadline(self.deadline)?;
        if let Some(&existing) = self.private_nodes.get(&Rc::as_ptr(node)) {
            return Ok(existing);
        }
        if node.timing.is_some() || node.materialization != Materialization::NotMaterialized {
            return Err(CandidateInventoryError::Unsupported {
                group: self.owner.group.0 as usize,
                candidate: self.owner.index,
                reason: "assigned execution timing or materialization",
            });
        }
        let mut inputs = Vec::new();
        node.operator.map_children(|child| {
            inputs.push(Rc::clone(child));
            Rc::clone(child)
        });
        let mut requirements = Vec::new();
        for input in inputs {
            let producers = if let Some(&group) = self.boundaries.get(&Rc::as_ptr(&input)) {
                self.used_boundaries.insert(group);
                let key = MemoGroupOutputKey::new(group, &input)
                    .map_err(CandidateInventoryError::InvalidMemo)?;
                if let Some(compatible) = self.compatible_producers_cache.get(&key) {
                    compatible
                } else {
                    let mut compatible = Vec::new();
                    for &candidate in
                        &self.inventory.implementation_roots_of_group[group.0 as usize]
                    {
                        super::check_deadline(self.deadline)?;
                        let template = &self.inventory.operator_nodes_of_candidates_for_memo_groups
                            [candidate.0]
                            .physical_node;
                        if compatible_output(&input, template)? {
                            compatible.push(candidate);
                        }
                    }
                    self.compatible_producers_cache.remember(key, compatible)
                }
            } else {
                vec![self.add(&input, None)?].into()
            };
            requirements.push(producers);
        }
        let id = if let Some(id) = reserved {
            self.inventory.producer_nodes_per_input[id.0] = requirements;
            id
        } else {
            let id = PhysicalCandidateId(
                self.inventory
                    .operator_nodes_of_candidates_for_memo_groups
                    .len(),
            );
            self.inventory.producer_nodes_per_input.push(requirements);
            self.inventory
                .operator_nodes_of_candidates_for_memo_groups
                .push(OperatorNodeOfCandidateForMemoGroup {
                    candidates_for_memo_group: vec![self.owner],
                    is_implementation_root: false,
                    accuracy_budget_parts: self.owner_accuracy_budget_parts,
                    physical_node: Rc::clone(node),
                });
            id
        };
        self.private_nodes.insert(Rc::as_ptr(node), id);
        Ok(id)
    }
}

fn compatible_output(
    expected: &OperatorNode,
    producer: &OperatorNode,
) -> Result<bool, CandidateInventoryError> {
    super::reconstruction::output_contract_satisfied(expected, producer)
        .map_err(CandidateInventoryError::InvalidMemo)
}
