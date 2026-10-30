//! Memoize identical supplier interfaces and quotient equivalent terminal readers.
//! Scope copies preserve independent bindings; internal computations are never merged.
use std::collections::HashMap;

use asap_types::ir::OperatorNode;
use asap_types::stages::MemoGroupId;

use asap_types::stages::PhysicalCandidateId;
use asap_types::stages::{SupplierCandidates, SupplierSetInterner};

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
    results: HashMap<MemoGroupOutputKey, SupplierCandidates>,
    lists: SupplierSetInterner,
    #[cfg(feature = "test-support")]
    disabled: bool,
    #[cfg(feature = "test-support")]
    hits: std::cell::Cell<usize>,
}

impl MemoGroupOutputToCompatibleProducersCache {
    pub fn get(&self, key: &MemoGroupOutputKey) -> Option<SupplierCandidates> {
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
        suppliers: Vec<PhysicalCandidateId>,
    ) -> SupplierCandidates {
        let suppliers = self.lists.intern(suppliers);
        self.results.insert(key, suppliers.clone());
        suppliers
    }
}

/// Quotient only terminal unary summary readers. Internal consumers and merge
/// operators retain their original identities. Scope expansion must retain one
/// independently bound reader copy per possible output reference; then every
/// old selection maps to these copies, and each new binding expands to an old
/// reader with the same complete interface and fixed local cost.
///
/// These operators bypass ConcreteRows' witness-based row certification.
/// certified_price checks every supplied producer; template-based proposal
/// prices remain advisory. Execution and accuracy are recomputed from selected
/// suppliers, never from the representative template's child witness.
///
/// `memo_candidate_roots` (root IDs, by memo group and candidate) is mapped
/// to the new IDs without removing duplicates.
pub fn factor_terminal_readers(
    inventory: &mut asap_types::stages::CandidateInventory,
    memo_candidate_roots: &mut [Vec<PhysicalCandidateId>],
    deadline: Option<std::time::Instant>,
) -> Result<usize, super::inventory::CandidateInventoryError> {
    use super::inventory::CandidateInventoryError;
    use asap_types::ir::ASAPOp;
    let invalid = |error: String| CandidateInventoryError::InvalidMemo(error);
    if !inventory.graph.reader_constraints.is_empty()
        || !inventory.graph.resource_budgets.is_empty()
    {
        return Ok(0);
    }
    let count = inventory.graph.candidates.len();
    let mut internal = vec![false; count];
    for candidate in &inventory.graph.candidates {
        super::check_deadline(deadline)?;
        for input in &candidate.producer_requirements {
            for producer in &input.compatible_producers {
                internal[producer.0] = true;
            }
        }
    }
    let mut outputs = vec![Vec::new(); count];
    for (output, root) in inventory.graph.query_roots.iter().enumerate() {
        for id in &root.compatible_candidates {
            outputs[id.0].push(output);
        }
    }
    let mut classes = HashMap::<Vec<u8>, usize>::new();
    let mut representative: Vec<_> = (0..count).collect();
    let mut eligible = vec![false; count];
    for id in 0..count {
        super::check_deadline(deadline)?;
        let candidate = &inventory.graph.candidates[id];
        if internal[id] || outputs[id].is_empty() || candidate.producer_requirements.len() != 1 {
            continue;
        }
        let Some(template) = &inventory.templates[id] else {
            continue;
        };
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
            candidate.group.0,
            candidate.local_cost.to_bits(),
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
    for (id, &rep) in representative.iter().enumerate() {
        super::check_deadline(deadline)?;
        if eligible[id] {
            let suppliers = unions.entry(rep).or_default();
            let known = known.entry(rep).or_default();
            for &supplier in
                &inventory.graph.candidates[id].producer_requirements[0].compatible_producers
            {
                if known.insert(supplier) {
                    suppliers.push(supplier);
                }
            }
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
    let mut lists = SupplierSetInterner::default();
    let mut candidates = Vec::with_capacity(next);
    let mut templates = Vec::with_capacity(next);
    for (id, candidate) in inventory.graph.candidates.iter().enumerate() {
        super::check_deadline(deadline)?;
        if representative[id] != id {
            continue;
        }
        let mut candidate = candidate.clone();
        if let Some(suppliers) = unions.remove(&id) {
            candidate.producer_requirements[0].compatible_producers = suppliers.into();
        }
        for input in &mut candidate.producer_requirements {
            input.compatible_producers = lists.intern(
                input
                    .compatible_producers
                    .iter()
                    .map(|id| new_ids[id.0])
                    .collect::<Vec<_>>(),
            );
        }
        candidates.push(candidate);
        templates.push(inventory.templates[id].clone());
    }
    let remap = |ids: &mut Vec<PhysicalCandidateId>| {
        let mut seen = std::collections::HashSet::new();
        *ids = ids
            .iter()
            .map(|id| new_ids[id.0])
            .filter(|id| seen.insert(*id))
            .collect();
    };
    for root in &mut inventory.graph.query_roots {
        remap(&mut root.compatible_candidates);
    }
    for group in &mut inventory.group_candidates {
        remap(group);
    }
    for id in memo_candidate_roots.iter_mut().flatten() {
        *id = new_ids[id.0];
    }
    inventory.graph.candidates = candidates;
    inventory.templates = templates;
    inventory
        .graph
        .validate()
        .map_err(|error| invalid(error.to_string()))?;
    Ok(removed)
}
