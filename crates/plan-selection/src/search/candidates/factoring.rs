//! Reuse the result of an identical typed supplier-compatibility subproblem.
//! This factors search construction, not physical instances: every candidate ID
//! and ordered input position remains independently selectable.
use std::collections::HashMap;

use asap_logical_asap_optimization::memo::MemoGroupId;
use asap_types::ir::OperatorNode;

use super::graph::PhysicalCandidateId;
use super::suppliers::{SupplierCandidates, SupplierSetInterner};

#[derive(Hash, PartialEq, Eq)]
pub(super) struct CompatibilityKey(Vec<u8>);

impl CompatibilityKey {
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
pub(super) struct CompatibilityMemo {
    results: HashMap<CompatibilityKey, SupplierCandidates>,
    lists: SupplierSetInterner,
    #[cfg(test)]
    disabled: bool,
    #[cfg(test)]
    hits: std::cell::Cell<usize>,
}

impl CompatibilityMemo {
    pub(super) fn get(&self, key: &CompatibilityKey) -> Option<SupplierCandidates> {
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
        key: CompatibilityKey,
        suppliers: Vec<PhysicalCandidateId>,
    ) -> SupplierCandidates {
        let suppliers = self.lists.intern(suppliers);
        self.results.insert(key, suppliers.clone());
        suppliers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asap_logical_asap_optimization::test_support::lower_promql;
    use asap_types::ir::properties::AccuracyTarget;

    // Logical ownership (including window semantics), closedness and required
    // keys remain distinct even when their fields have identical types.
    #[test]
    fn compatibility_cache_keeps_every_expected_output_requirement() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let group = MemoGroupId(1);
        let key = CompatibilityKey::new(group, &expected).unwrap();
        let mut memo = CompatibilityMemo::default();
        let suppliers = vec![PhysicalCandidateId(3), PhysicalCandidateId(7)];
        memo.remember(key, suppliers.clone());
        assert_eq!(
            &*memo
                .get(&CompatibilityKey::new(group, &expected).unwrap())
                .unwrap(),
            &suppliers
        );
        assert!(memo
            .get(&CompatibilityKey::new(MemoGroupId(2), &expected).unwrap())
            .is_none());
        let mut closed = (*expected).clone();
        closed.schema.closed = !closed.schema.closed;
        assert!(memo
            .get(&CompatibilityKey::new(group, &closed).unwrap())
            .is_none());
        let mut keys = (*expected).clone();
        keys.schema.unique_keys.push(vec![]);
        assert!(memo
            .get(&CompatibilityKey::new(group, &keys).unwrap())
            .is_none());
    }

    // Storage sharing never couples the two consumers' supplier decisions.
    #[test]
    fn two_reader_bindings_keep_independent_supplier_lists() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let key = || CompatibilityKey::new(MemoGroupId(1), &expected).unwrap();
        let mut memo = CompatibilityMemo::default();
        memo.remember(key(), vec![PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        let mut first = memo.get(&key()).unwrap();
        let second = memo.get(&key()).unwrap();
        first.retain(|id| id.0 == 1);
        assert_eq!(&*first, &[PhysicalCandidateId(1)]);
        assert_eq!(&*second, &[PhysicalCandidateId(1), PhysicalCandidateId(2)]);
        assert_eq!(memo.get(&key()).unwrap(), second);
    }
    // Exhaust every two-reader binding, charging each retained producer once.
    // A longer retention requirement has its own logical group and cannot
    // accidentally inherit the shorter reader's supplier set from the cache.
    #[test]
    fn cached_interfaces_preserve_exhaustive_sharing_and_retention_choices() {
        let expected = lower_promql("latency", AccuracyTarget::exact());
        let short = vec![PhysicalCandidateId(0), PhysicalCandidateId(1)];
        let long = vec![PhysicalCandidateId(1)];
        let mut memo = CompatibilityMemo::default();
        let key = |group| CompatibilityKey::new(MemoGroupId(group), &expected).unwrap();
        memo.remember(key(0), short.clone());
        memo.remember(key(1), long.clone());
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
            let factored = enumerate(&memo.get(&key(0)).unwrap(), &memo.get(&key(group)).unwrap());
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
