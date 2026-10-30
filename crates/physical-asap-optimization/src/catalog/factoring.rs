//! Memoize identical producer interfaces and quotient equivalent terminal readers.
//! Internal computations are never merged.
use std::collections::HashMap;

use asap_types::ir::OperatorNode;
use asap_types::stages::MemoGroupId;

use asap_types::stages::{CandidateForMemoGroupId, PhysicalCandidateId};
use asap_types::stages::{ProducerCandidates, ProducerSetInterner};

#[derive(Hash, PartialEq, Eq)]
pub struct MemoGroupOutputKey(Vec<u8>);

impl MemoGroupOutputKey {
    /// Include everything output_contract_satisfied reads from its expected
    /// node. Byte-exact keys preserve parameters that PartialEq or JSON may
    /// erase, and include closedness and every required uniqueness key.
    pub fn new(group: MemoGroupId, expected: &OperatorNode) -> Result<Self, String> {
        rmp_serde::to_vec(&(group.0, &expected.result_kind, &expected.schema))
            .map(Self)
            .map_err(|error| error.to_string())
    }
}

#[derive(Default)]
pub struct MemoGroupOutputToCompatibleProducersCache {
    results: HashMap<MemoGroupOutputKey, ProducerCandidates>,
    lists: ProducerSetInterner,
    #[cfg(feature = "test-support")]
    disabled: bool,
    #[cfg(feature = "test-support")]
    hits: std::cell::Cell<usize>,
}

impl MemoGroupOutputToCompatibleProducersCache {
    pub fn get(&self, key: &MemoGroupOutputKey) -> Option<ProducerCandidates> {
        #[cfg(feature = "test-support")]
        if self.disabled {
            return None;
        }
        let found = self.results.get(key).cloned();
        #[cfg(feature = "test-support")]
        if found.is_some() {
            self.hits.set(self.hits.get() + 1);
        }
        found
    }

    #[cfg(feature = "test-support")]
    pub fn hits(&self) -> usize {
        self.hits.get()
    }

    #[cfg(feature = "test-support")]
    pub fn disabled() -> Self {
        Self {
            disabled: true,
            ..Self::default()
        }
    }

    pub fn remember(
        &mut self,
        key: MemoGroupOutputKey,
        producer_candidates: Vec<PhysicalCandidateId>,
    ) -> ProducerCandidates {
        let producer_candidates = self.lists.intern(producer_candidates);
        self.results.insert(key, producer_candidates.clone());
        producer_candidates
    }
}

/// Quotient only terminal unary summary readers. Internal consumers and merge
/// operators retain their original identities. A selection binds the
/// factored reader independently per output reference (one selected node
/// each), so every selection of the unfactored readers maps to one of the
/// factored reader, and each binding of it is an unfactored reader with the
/// same complete interface.
///
/// These operators bypass ConcreteRows' witness-based row certification.
/// certified_price checks every supplied producer; template-based proposal
/// prices remain advisory. Execution and accuracy are recomputed from selected
/// producers, never from the representative template's child witness.
///
/// A kept node's `candidates_for_memo_group` becomes the union, ascending,
/// of the candidates of the nodes factored into it.
pub fn factor_terminal_readers(
    inventory: &mut super::inventory::CandidateInventory,
    deadline: Option<std::time::Instant>,
) -> Result<usize, super::inventory::CandidateInventoryError> {
    use super::inventory::CandidateInventoryError;
    use asap_types::ir::ASAPOp;
    let invalid = |error: String| CandidateInventoryError::InvalidMemo(error);
    let count = inventory.operator_nodes_of_candidates_for_memo_groups.len();
    let mut internal = vec![false; count];
    for inputs in &inventory.producer_nodes_per_input {
        super::check_deadline(deadline)?;
        for input in inputs {
            for producer in input {
                internal[producer.0] = true;
            }
        }
    }
    let mut outputs = vec![Vec::new(); count];
    for (output, root) in inventory.query_output_nodes.iter().enumerate() {
        for id in root {
            outputs[id.0].push(output);
        }
    }
    let mut classes = HashMap::<Vec<u8>, usize>::new();
    let mut representative: Vec<_> = (0..count).collect();
    let mut eligible = vec![false; count];
    for id in 0..count {
        super::check_deadline(deadline)?;
        let node = &inventory.operator_nodes_of_candidates_for_memo_groups[id];
        if internal[id]
            || outputs[id].is_empty()
            || inventory.producer_nodes_per_input[id].len() != 1
        {
            continue;
        }
        let template = &node.physical_node;
        if !matches!(
            template.asap(),
            Some(ASAPOp::SummaryEstimate { .. } | ASAPOp::FinalizeExactAccumulator { .. })
        ) {
            continue;
        }
        let mut inputs = Vec::new();
        let operator = template.operator.map_children(|child| {
            inputs.push(child.clone());
            0u8
        });
        let [input] = inputs.as_slice() else {
            continue;
        };
        eligible[id] = true;
        let key = rmp_serde::to_vec(&(
            node.group().0,
            &outputs[id],
            operator,
            &template.schema,
            template.result_kind,
            &template.guarantee,
            template.timing,
            template.materialization,
            &input.schema,
            input.result_kind,
            &input.guarantee,
            input.timing,
            input.materialization,
        ))
        .map_err(|error| invalid(error.to_string()))?;
        if let Some(&previous) = classes.get(&key) {
            representative[id] = previous;
        } else {
            classes.insert(key, id);
        }
    }
    let removed = representative
        .iter()
        .enumerate()
        .filter(|(id, rep)| *id != **rep)
        .count();
    if removed == 0 {
        return Ok(0);
    }
    let mut unions = HashMap::<usize, Vec<PhysicalCandidateId>>::new();
    let mut known = HashMap::<usize, std::collections::HashSet<PhysicalCandidateId>>::new();
    let mut candidates_for_memo_group = HashMap::<usize, Vec<CandidateForMemoGroupId>>::new();
    for (id, &rep) in representative.iter().enumerate() {
        super::check_deadline(deadline)?;
        if eligible[id] {
            let producer_candidates = unions.entry(rep).or_default();
            let known = known.entry(rep).or_default();
            for &producer_id in &inventory.producer_nodes_per_input[id][0] {
                if known.insert(producer_id) {
                    producer_candidates.push(producer_id);
                }
            }
        }
        if id != rep {
            candidates_for_memo_group.entry(rep).or_default().extend(
                &inventory.operator_nodes_of_candidates_for_memo_groups[id]
                    .candidates_for_memo_group,
            );
        }
    }
    let mut new_ids = vec![PhysicalCandidateId(0); count];
    let mut next = 0;
    for (id, &rep) in representative.iter().enumerate() {
        if id == rep {
            new_ids[id] = PhysicalCandidateId(next);
            next += 1;
        }
    }
    for (id, &rep) in representative.iter().enumerate() {
        new_ids[id] = new_ids[rep];
    }
    let mut lists = ProducerSetInterner::default();
    let mut nodes = Vec::with_capacity(next);
    let mut producer_nodes_per_input = Vec::with_capacity(next);
    for (id, node) in inventory
        .operator_nodes_of_candidates_for_memo_groups
        .iter()
        .enumerate()
    {
        super::check_deadline(deadline)?;
        if representative[id] != id {
            continue;
        }
        let mut node = node.clone();
        if let Some(factored) = candidates_for_memo_group.remove(&id) {
            node.candidates_for_memo_group.extend(factored);
            node.candidates_for_memo_group.sort_unstable();
            node.candidates_for_memo_group.dedup();
        }
        let mut inputs = inventory.producer_nodes_per_input[id].clone();
        if let Some(producer_candidates) = unions.remove(&id) {
            inputs[0] = producer_candidates.into();
        }
        for input in &mut inputs {
            *input = lists.intern(input.iter().map(|id| new_ids[id.0]).collect::<Vec<_>>());
        }
        nodes.push(node);
        producer_nodes_per_input.push(inputs);
    }
    let remap = |ids: &mut Vec<PhysicalCandidateId>| {
        let mut seen = std::collections::HashSet::new();
        *ids = ids
            .iter()
            .map(|id| new_ids[id.0])
            .filter(|id| seen.insert(*id))
            .collect();
    };
    for root in &mut inventory.query_output_nodes {
        remap(root);
    }
    for group in &mut inventory.implementation_roots_of_group {
        remap(group);
    }
    inventory.operator_nodes_of_candidates_for_memo_groups = nodes;
    inventory.producer_nodes_per_input = producer_nodes_per_input;
    inventory.validate().map_err(invalid)?;
    Ok(removed)
}
