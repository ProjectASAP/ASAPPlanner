//! Memoize identical supplier interfaces and quotient equivalent terminal readers.
//! Scope copies preserve independent bindings; internal computations are never merged.
use std::collections::HashMap;

use asap_logical_asap_optimization::pass2::memo::MemoGroupId;
use asap_types::ir::OperatorNode;

use super::graph::PhysicalCandidateId;
use super::suppliers::{SupplierCandidates, SupplierSetInterner};

#[derive(Hash, PartialEq, Eq)]
pub(super) struct MemoGroupOutputKey(Vec<u8>);

impl MemoGroupOutputKey {
    /// Include everything output_contract_satisfied reads from its expected
    /// node. Byte-exact keys preserve parameters that PartialEq or JSON may
    /// erase, and include closedness and every required uniqueness key.
    pub(super) fn new(group: MemoGroupId, expected: &OperatorNode) -> Result<Self, String> {
        rmp_serde::to_vec(&(group.0, &expected.result_kind, &expected.schema))
            .map(Self)
            .map_err(|error| error.to_string())
    }
}

#[derive(Default)]
pub(super) struct MemoGroupOutputToCompatibleProducersCache {
    results: HashMap<MemoGroupOutputKey, SupplierCandidates>,
    lists: SupplierSetInterner,
    #[cfg(test)]
    disabled: bool,
    #[cfg(test)]
    hits: std::cell::Cell<usize>,
}

impl MemoGroupOutputToCompatibleProducersCache {
    pub(super) fn get(&self, key: &MemoGroupOutputKey) -> Option<SupplierCandidates> {
        #[cfg(test)]
        if self.disabled {
            return None;
        }
        let found = self.results.get(key).cloned();
        #[cfg(test)]
        if found.is_some() {
            self.hits.set(self.hits.get() + 1);
        }
        found
    }

    #[cfg(test)]
    pub(super) fn hits(&self) -> usize {
        self.hits.get()
    }

    #[cfg(test)]
    pub(super) fn disabled() -> Self {
        Self {
            disabled: true,
            ..Self::default()
        }
    }

    pub(super) fn remember(
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
pub(super) fn factor_terminal_readers(
    inventory: &mut super::inventory::CandidateInventory,
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

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    fn terminal_fixture() -> super::super::inventory::CandidateInventory {
        use super::super::graph::{
            CandidateDependencyGraph, PhysicalCandidate, ProducerRequirement, QueryRootCandidates,
        };
        use asap_logical_asap_optimization::test_support::{cms_summary, estimate_over};
        use asap_types::ir::properties::AccuracyGuarantee;
        use std::rc::Rc;
        let state = Rc::new(cms_summary("count(m)"));
        let reader = estimate_over(state.clone(), AccuracyGuarantee::exact("fixture"));
        super::super::inventory::CandidateInventory {
            graph: CandidateDependencyGraph {
                candidates: (0..4)
                    .map(|id| PhysicalCandidate {
                        group: MemoGroupId(if id < 2 { 0 } else { 1 }),
                        local_cost: [8.0, 12.0, 1.0, 1.0][id],
                        producer_requirements: if id < 2 {
                            vec![]
                        } else {
                            vec![ProducerRequirement {
                                compatible_producers: vec![PhysicalCandidateId(id - 2)].into(),
                            }]
                        },
                    })
                    .collect(),
                query_roots: vec![
                    QueryRootCandidates {
                        compatible_candidates: vec![PhysicalCandidateId(2), PhysicalCandidateId(3)]
                    };
                    2
                ],
                ..Default::default()
            },
            templates: vec![
                Some(state.clone()),
                Some(state),
                Some(reader.clone()),
                Some(reader),
            ],
            group_candidates: vec![
                vec![PhysicalCandidateId(0), PhysicalCandidateId(1)],
                vec![PhysicalCandidateId(2), PhysicalCandidateId(3)],
            ],
        }
    }

    fn with_scopes(
        inventory: super::super::inventory::CandidateInventory,
    ) -> super::super::configurations::CandidateConfigurationInventory {
        use super::super::configurations::{
            CandidateConfiguration, CandidateConfigurationInventory,
        };
        use asap_types::ir::properties::ExecutionTiming;
        use asap_types::physical::{Materialization, Medium};
        let count = inventory.graph.candidates.len();
        let configurations = (0..count)
            .map(|id| CandidateConfiguration {
                source_candidate: PhysicalCandidateId(id),
                timing: if id < 2 {
                    ExecutionTiming::IngestionTime
                } else {
                    ExecutionTiming::QueryTime
                },
                materialization: if id < 2 {
                    Materialization::MatAtIngestion(Medium::Memory)
                } else {
                    Materialization::NotMaterialized
                },
            })
            .collect();
        super::super::scopes::expand_execution_scopes(
            CandidateConfigurationInventory {
                inventory,
                configurations,
            },
            &vec![1; count],
            &[0, 0],
        )
        .unwrap()
    }

    // Enumerate every output-to-state assignment, including split bindings for
    // two references of one query. State 1 models a higher-retention producer:
    // its different cost and identity must survive despite equal reader types.
    #[test]
    fn terminal_quotient_preserves_all_bindings_and_pay_once_costs() {
        use super::super::graph::ReaderConstraint;
        use crate::search::milp::model::select_candidates;
        for assignment in [[0, 0], [0, 1], [1, 0], [1, 1]] {
            let original = with_scopes(terminal_fixture());
            let mut factored = terminal_fixture();
            assert_eq!(
                factor_terminal_readers(&mut factored, &mut [], None).unwrap(),
                1
            );
            assert_eq!(factored.graph.candidates.len(), 3);
            let compressed = with_scopes(factored);
            assert!(
                compressed.inventory.graph.candidates.len()
                    < original.inventory.graph.candidates.len()
            );
            for mut configured in [original, compressed] {
                for state in 0..2 {
                    configured.inventory.graph.reader_constraints.push(
                        ReaderConstraint::AllowedOutputs {
                            candidate: PhysicalCandidateId(state),
                            outputs: assignment
                                .iter()
                                .enumerate()
                                .filter_map(|(output, &producer)| {
                                    (producer == state).then_some(output)
                                })
                                .collect(),
                        },
                    );
                }
                let selected = select_candidates(&configured.inventory.graph)
                    .unwrap()
                    .expect("every original assignment remains feasible");
                let expected = if assignment[0] == assignment[1] {
                    [8.0, 12.0][assignment[0]] + 1.0
                } else {
                    22.0
                };
                assert_eq!(selected.modeled_cost, expected, "assignment {assignment:?}");
            }
        }
    }

    #[test]
    fn duplicate_terminal_interfaces_reduce_scoped_candidates_and_connections() {
        let build = || {
            let mut inventory = terminal_fixture();
            for reader in &mut inventory.graph.candidates[2..] {
                reader.producer_requirements[0].compatible_producers =
                    vec![PhysicalCandidateId(0), PhysicalCandidateId(1)].into();
            }
            inventory
        };
        let original = with_scopes(build());
        let mut compact = build();
        assert_eq!(
            factor_terminal_readers(&mut compact, &mut [], None).unwrap(),
            1
        );
        let compact = with_scopes(compact);
        let counts = |graph: &super::super::graph::CandidateDependencyGraph| {
            (
                graph.candidates.len(),
                graph
                    .candidates
                    .iter()
                    .flat_map(|candidate| &candidate.producer_requirements)
                    .map(|input| input.compatible_producers.len())
                    .sum::<usize>(),
            )
        };
        assert_eq!(counts(&original.inventory.graph), (6, 8));
        assert_eq!(counts(&compact.inventory.graph), (4, 4));
    }

    #[test]
    fn terminal_quotient_preserves_interfaces_and_internal_consumers() {
        use super::super::graph::ProducerRequirement;
        use asap_types::ir::properties::AccuracyGuarantee;
        use std::rc::Rc;
        let mut accuracy = terminal_fixture();
        let mut other = (**accuracy.templates[3].as_ref().unwrap()).clone();
        other.guarantee = Some(AccuracyGuarantee::exact("different proof contract"));
        accuracy.templates[3] = Some(Rc::new(other));
        assert_eq!(
            factor_terminal_readers(&mut accuracy, &mut [], None).unwrap(),
            0
        );
        let mut eligibility = terminal_fixture();
        eligibility.graph.query_roots[1].compatible_candidates = vec![PhysicalCandidateId(3)];
        assert_eq!(
            factor_terminal_readers(&mut eligibility, &mut [], None).unwrap(),
            0
        );
        let mut internal = terminal_fixture();
        internal.graph.candidates[0]
            .producer_requirements
            .push(ProducerRequirement {
                compatible_producers: vec![PhysicalCandidateId(2)].into(),
            });
        assert_eq!(
            factor_terminal_readers(&mut internal, &mut [], None).unwrap(),
            0
        );
    }

    // Logical ownership (including window semantics), closedness and required
    // keys remain distinct even when their fields have identical types.
    #[test]
    fn compatibility_cache_keeps_every_expected_output_requirement() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let group = MemoGroupId(1);
        let key = MemoGroupOutputKey::new(group, &expected).unwrap();
        let mut cache = MemoGroupOutputToCompatibleProducersCache::default();
        let suppliers = vec![PhysicalCandidateId(3), PhysicalCandidateId(7)];
        cache.remember(key, suppliers.clone());
        assert_eq!(
            &*cache
                .get(&MemoGroupOutputKey::new(group, &expected).unwrap())
                .unwrap(),
            &suppliers
        );
        assert!(cache
            .get(&MemoGroupOutputKey::new(MemoGroupId(2), &expected).unwrap())
            .is_none());
        let mut closed = (*expected).clone();
        closed.schema.closed = !closed.schema.closed;
        assert!(cache
            .get(&MemoGroupOutputKey::new(group, &closed).unwrap())
            .is_none());
        let mut keys = (*expected).clone();
        keys.schema.unique_keys.push(vec![]);
        assert!(cache
            .get(&MemoGroupOutputKey::new(group, &keys).unwrap())
            .is_none());
    }

    // Storage sharing never couples the two consumers' supplier decisions.
    #[test]
    fn two_reader_bindings_keep_independent_supplier_lists() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let key = || MemoGroupOutputKey::new(MemoGroupId(1), &expected).unwrap();
        let mut cache = MemoGroupOutputToCompatibleProducersCache::default();
        cache.remember(key(), vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        let mut first = cache.get(&key()).unwrap();
        let second = cache.get(&key()).unwrap();
        first.retain(|id| id.0 == 1);
        assert_eq!(&*first, &[PhysicalCandidateId(1)]);
        assert_eq!(&*second, &[PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        assert_eq!(cache.get(&key()).unwrap(), second);
    }
    // Exhaust every two-reader binding, charging each retained producer once.
    // A longer retention requirement has its own logical group and cannot
    // accidentally inherit the shorter reader's supplier set from the cache.
    #[test]
    fn cached_interfaces_preserve_exhaustive_sharing_and_retention_choices() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let short = vec![PhysicalCandidateId(0), PhysicalCandidateId(1)];
        let long = vec![PhysicalCandidateId(1)];
        let mut cache = MemoGroupOutputToCompatibleProducersCache::default();
        let key = |group| MemoGroupOutputKey::new(MemoGroupId(group), &expected).unwrap();
        cache.remember(key(0), short.clone());
        cache.remember(key(1), long.clone());
        let enumerate = |left: &[PhysicalCandidateId], right: &[PhysicalCandidateId]| {
            let costs = [8, 12];
            left.iter()
                .flat_map(|a| {
                    right.iter().map(move |b| {
                        let cost = costs[a.0] + if a == b { 0 } else { costs[b.0] } + 2;
                        ((a.0, b.0), cost)
                    })
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        for (right, group, optimum) in [(&short, 0, 10), (&long, 1, 14)] {
            let reference = enumerate(&short, right);
            let factored = enumerate(
                &cache.get(&key(0)).unwrap(),
                &cache.get(&key(group)).unwrap(),
            );
            assert_eq!(factored, reference);
            assert_eq!(factored.values().min(), Some(&optimum));
            assert_eq!(
                factored.get(&(0, 1)),
                Some(&22),
                "two distinct producer bindings remain feasible"
            );
        }
    }
}
