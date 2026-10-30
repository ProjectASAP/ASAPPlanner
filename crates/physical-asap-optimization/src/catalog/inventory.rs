//! Lower the logical candidate memo into typed immediate candidate connections.
//!
//! The callback supplies node-local costs for a fixed accounting context.
//! Consumer-dependent demand, composed accuracy, and physical materialization
//! are handled by later configuration and evaluation stages. Private exact
//! operators are lowered before decomposition; the resulting graph still needs
//! Stage 2 validation before execution.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use asap_types::ir::OperatorNode;
use asap_types::physical::Materialization;
use asap_types::stages::CandidateMemo;
use asap_types::stages::MemoGroupId;

use super::factoring::{MemoGroupOutputKey, MemoGroupOutputToCompatibleProducersCache};
use asap_types::stages::{
    CandidateDependencyGraph, CandidateInventory, PhysicalCandidate, PhysicalCandidateId,
    ProducerRequirement, QueryRootCandidates,
};

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
    #[error("node-local fixed-context cost: {0}")]
    Cost(String),
}

/// The inventory, and the root ID of each memo candidate, indexed like
/// `CandidateMemo::candidates` (factoring may give several the same ID).
pub fn build_candidate_inventory_before(
    memo: &CandidateMemo,
    output_groups: &[MemoGroupId],
    node_local_cost: impl FnMut(&OperatorNode) -> Result<f64, String>,
    deadline: Option<std::time::Instant>,
) -> Result<(CandidateInventory, Vec<Vec<PhysicalCandidateId>>), CandidateInventoryError> {
    let mut inventory = build_with_compatible_producers_cache(
        memo,
        output_groups,
        node_local_cost,
        deadline,
        &mut MemoGroupOutputToCompatibleProducersCache::default(),
    )?;
    let mut memo_candidate_roots = inventory.group_candidates.clone();
    super::factoring::factor_terminal_readers(&mut inventory, &mut memo_candidate_roots, deadline)?;
    Ok((inventory, memo_candidate_roots))
}

pub fn build_with_compatible_producers_cache(
    memo: &CandidateMemo,
    output_groups: &[MemoGroupId],
    mut node_local_cost: impl FnMut(&OperatorNode) -> Result<f64, String>,
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
        graph: CandidateDependencyGraph::default(),
        templates: Vec::new(),
        group_candidates: vec![Vec::new(); memo.groups.len()],
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
            // not additional results it can supply. Private-node decomposition
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
            let id = PhysicalCandidateId(inventory.graph.candidates.len());
            inventory.group_candidates[group].push(id);
            inventory.templates.push(Some(implemented));
            inventory.graph.candidates.push(PhysicalCandidate {
                group: MemoGroupId(group as u32),
                local_cost: 0.0,
                producer_requirements: Vec::new(),
            });
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
                owner: MemoGroupId(group as u32),
                candidate_index: index,
                node_local_cost: &mut node_local_cost,
                deadline,
            };
            let root_id = builder.inventory.group_candidates[group][index];
            let implemented = builder.inventory.templates[root_id.0]
                .as_ref()
                .cloned()
                .ok_or_else(|| invalid("missing implemented root template"))?;
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
        for &id in &inventory.group_candidates[group.0 as usize] {
            let template = inventory.templates[id.0]
                .as_ref()
                .ok_or_else(|| invalid("missing root template"))?;
            if compatible_output(expected, template)? {
                compatible_candidates.push(id);
            }
        }
        inventory.graph.query_roots.push(QueryRootCandidates {
            compatible_candidates,
        });
    }
    inventory
        .graph
        .validate()
        .map_err(|error| invalid(&error.to_string()))?;
    super::check_deadline(deadline)?;
    Ok(inventory)
}

struct ImmediateCandidateBuilder<'a, F> {
    inventory: &'a mut CandidateInventory,
    compatible_producers_cache: &'a mut MemoGroupOutputToCompatibleProducersCache,
    boundaries: HashMap<*const OperatorNode, MemoGroupId>,
    private_nodes: HashMap<*const OperatorNode, PhysicalCandidateId>,
    used_boundaries: HashSet<MemoGroupId>,
    owner: MemoGroupId,
    candidate_index: usize,
    node_local_cost: &'a mut F,
    deadline: Option<std::time::Instant>,
}

impl<F: FnMut(&OperatorNode) -> Result<f64, String>> ImmediateCandidateBuilder<'_, F> {
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
                group: self.owner.0 as usize,
                candidate: self.candidate_index,
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
                    for &candidate in &self.inventory.group_candidates[group.0 as usize] {
                        super::check_deadline(self.deadline)?;
                        let template =
                            self.inventory.templates[candidate.0]
                                .as_ref()
                                .ok_or_else(|| {
                                    CandidateInventoryError::InvalidMemo(
                                        "missing root template".into(),
                                    )
                                })?;
                        if compatible_output(&input, template)? {
                            compatible.push(candidate);
                        }
                    }
                    self.compatible_producers_cache.remember(key, compatible)
                }
            } else {
                vec![self.add(&input, None)?].into()
            };
            requirements.push(ProducerRequirement {
                compatible_producers: producers,
            });
        }
        let cost = (self.node_local_cost)(node).map_err(CandidateInventoryError::Cost)?;
        if !cost.is_finite() || cost < 0.0 {
            return Err(CandidateInventoryError::Cost(
                "cost must be finite and nonnegative".into(),
            ));
        }
        let candidate = PhysicalCandidate {
            group: self.owner,
            local_cost: cost,
            producer_requirements: requirements,
        };
        let id = if let Some(id) = reserved {
            self.inventory.graph.candidates[id.0] = candidate;
            id
        } else {
            let id = PhysicalCandidateId(self.inventory.graph.candidates.len());
            self.inventory.graph.candidates.push(candidate);
            self.inventory.templates.push(Some(Rc::clone(node)));
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
